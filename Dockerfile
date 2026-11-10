# Тулчейн для сборки, тестов и замеров: Rust 1.99.1 на Debian 11 (bullseye).
#
# Почему не `rust:1.99.1-bookworm` напрямую. База образа задаёт МИНИМАЛЬНУЮ glibc готового
# бинаря: сборка на bookworm требует GLIBC_2.34 (замер по текущему бинарю) и не запускается
# на Debian 11 и Astra Linux, где glibc 2.31 — ошибка «GLIBC_2.34 not found» приходит уже
# у пользователя. Порог проверяется гейтом `make glibc-check`, а не выясняется из неё.
#
# Почему не тег `rust:1.99.1-bullseye`. Его не существует: официальный проект перестал
# публиковать bullseye-варианты после 1.90, а локальный образ с таким именем — чья-то ручная
# сборка, то есть зависимость от чужой машины (в соседнем `rust-rest-ldap` так и вышло:
# образ есть, Dockerfile не закоммичен). Поэтому образ собирается здесь и воспроизводимо.
#
# Тулчейн КОПИРУЕТСЯ из официального slim-bookworm, а не ставится rustup'ом: официальные
# сборки Rust линкованы против glibc 2.17 и на bullseye работают как есть, а libc в наших
# бинарях приходит из базы — 2.31. rustfmt и clippy — rustup-компоненты, их добавляем в
# bookworm-стадии: тарболы компонентов от libc не зависят, а собранные там бинари зависели бы.
#
# Заодно этот образ — тот же набор инструментов, что ставят шаги CI: rustfmt, clippy,
# cargo-deny, cargo-llvm-cov. Иначе `make` и Actions расходятся, и «зелено локально»
# перестаёт что-то значить.
FROM rust:1.99.1-slim-bookworm AS toolchain

# Все компоненты тулчейна ставятся ЗДЕСЬ, в стадии с рабочим rustup и сетью, и уезжают в
# финальный образ вместе с деревом `/usr/local/rustup` (копируется целиком).
#
# `llvm-tools-preview` долго стоял не здесь, а в финальном образе (`install-cargo-llvm-cov`), и
# его там не оказывалось: rustfmt и clippy из этой стадии присутствовали, а llvm-cov — нет.
# Следствие было не косметическим: `make cov` при каждом прогоне заново тянул компонент из сети
# (`info: downloading component llvm-tools`), спрашивал `Proceed? [Y/n]` и на неинтерактивном
# stdin просто ждал — на медленном канале это неотличимо от зависания.
RUN rustup component add rustfmt clippy llvm-tools-preview

FROM rust:1.90-bullseye

ARG RUST_TOOLCHAIN=1.99.1

# Деревья rustup и cargo целиком: в базе 1.90, и её собственный `/usr/local/bin/cargo` —
# симлинк внутрь этого дерева. Копия делает версию из `rust-toolchain.toml` версией по
# умолчанию, не удаляя базовую.
COPY --from=toolchain /usr/local/rustup /usr/local/rustup
COPY --from=toolchain /usr/local/cargo /usr/local/cargo
ENV RUSTUP_TOOLCHAIN=${RUST_TOOLCHAIN} \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# Bullseye вышел из LTS и уехал в архив: `deb.debian.org` отвечает 404 на свои же версии
# пакетов, и любой `apt-get install` падает кодом 100. Архив работает, но подписан
# просроченной датой — отсюда `Check-Valid-Until=false`.
RUN printf '%s\n' \
      'deb http://archive.debian.org/debian bullseye main' \
      'deb http://archive.debian.org/debian-security bullseye-security main' \
      > /etc/apt/sources.list

# `build-essential` для линковки, `pkg-config` и `ca-certificates` — для TLS-крейтов и установщиков.
RUN apt-get -o Acquire::Check-Valid-Until=false update \
 && apt-get install -y --no-install-recommends \
      build-essential pkg-config ca-certificates git \
 && rm -rf /var/lib/apt/lists/*

# Политика зависимостей и покрытие. Оба установщика качают статические (musl) бинари, поэтому
# на bullseye они запускаются без требования к glibc; версии и хеши закреплены в скриптах,
# чтобы локальные гейты и CI ставили одно и то же.
COPY scripts/install-cargo-deny.sh /usr/local/bin/install-cargo-deny
COPY scripts/install-cargo-llvm-cov.sh /usr/local/bin/install-cargo-llvm-cov

RUN sh /usr/local/bin/install-cargo-deny \
 && sh /usr/local/bin/install-cargo-llvm-cov

# Гейты утверждают наличие инструментов, а не обнаруживают в момент отчёта, что rustfmt
# не было и проверка молча ничего не делала. `llvm-cov` проверяется ДВУМЯ способами: командой
# (`cargo llvm-cov --version` — сам плагин) и файлом в sysroot (сам инструмент, без которого
# плагин уходит качать компонент из сети). Отсутствие любого из них роняет сборку образа, а не
# откладывается до первого `make cov`.
#
# `cargo-deny` и `cargo-mutants` здесь НЕ проверяются намеренно: они живут не в образе, а в томе
# cargo (`CARGO_HOME=/cargo` при запуске `make`), и в CI ставятся отдельными шагами. Утверждать
# в образе то, чего он не несёт, — значит либо ложно падать, либо (как было) не проверять вовсе.
RUN rustc --version && cargo --version && rustfmt --version && cargo clippy --version \
 && cargo llvm-cov --version \
 && llvm_cov="$(rustc --print sysroot)/lib/rustlib/$(rustc -vV | sed -n 's/^host: //p')/bin/llvm-cov" \
 && test -x "$llvm_cov" \
 && echo "llvm-cov: $llvm_cov"
