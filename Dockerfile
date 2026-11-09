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

RUN rustup component add rustfmt clippy

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
# не было и проверка молча ничего не делала.
RUN rustc --version && cargo --version && rustfmt --version && cargo clippy --version
