# Сборка и гейты — в контейнере rust:1.99.1-bullseye (Debian 11, glibc 2.31).
# На хосте rust-тулчейн не нужен: нужен только docker.
#
#   make            — fmt + clippy + deny + test + tokens + docs + glibc-check
#   make build      — release-бинарь в кэше, копия в ~/.local/bin
#   make act        — прогон GitHub Actions локально (nektos/act), тот же образ
#
# Почему bullseye, а не bookworm: база образа задаёт минимальную glibc готового
# бинаря. На bookworm это 2.34, и артефакт не стартует на Astra/Debian 11 (2.31) —
# ошибка приходит уже у пользователя. Порог держит гейт glibc-check.
#
# Тега rust:1.99.1-bullseye в Docker Hub нет (официальные bullseye-варианты кончились
# на 1.90): образ собран локально, и его рецепт лежит в Dockerfile — `make image`
# пересобирает его воспроизводимо на любой машине. Образ НЕ пересобирается по ходу
# обычных целей: он уже есть, а неявная пересборка 2.4 ГБ замедляет каждый прогон.
# Кэш cargo — общий с соседним rust-rest-ldap (том rrl-cargo-bullseye): реестр качается
# один раз на всю машину, а target у каждого проекта свой.

IMAGE       ?= rust:1.99.1-bullseye
CARGO_VOL   ?= rrl-cargo-bullseye
UID_GID     := $(shell id -u):$(shell id -g)
# PATH включает `$(CARGO_HOME)/bin` — иначе инструменты, поставленные в том (`cargo-deny`,
# `cargo-mutants`), не находятся: `cargo mutants` отвечает «no such command: mutants», хотя
# бинарь лежит рядом в томе. Так уже было: у цели `deny` PATH расширялся ВНУТРИ рецепта, а у
# `mutants`/`mutants-file` — нет, и мутационный гейт молча не работал (8 воркеров с rc=101 и
# итог «убито 0, выжило 0» — то есть ноль проверки, а не ноль мутантов). Расширение PATH здесь,
# в одном месте, распространяется на все цели разом.
DOCKER_FILE  = --rm -v $(CURDIR):/src -v $(CARGO_VOL):/cargo \
               -u $(UID_GID) -w /src \
               -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/cargo/target-tasq -e HOME=/cargo/home \
               -e PATH=/cargo/bin:/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
TARGET_DIR    = /cargo/target-tasq
RUN          = docker run $(DOCKER_FILE) $(IMAGE)

.PHONY: all image image-check cache-init fmt clippy deny test cov build install smoke act clean tokens docs glibc-check mutants mutants-file mutants-diff

all: fmt clippy deny test tokens docs glibc-check

# Пересборка образа из репозитория — отдельным действием, а не побочным эффектом:
# официального тега нет, а «взять у соседа» означает зависимость от чужой машины.
image:
	docker build -q -t $(IMAGE) -f Dockerfile .

# Образ обязан быть: без него проверки падают на `pull access denied`, что читается как
# проблема реестра. Отказ громкий и с подсказкой — как и любой другой отказ конфигурации.
image-check:
	@docker image inspect $(IMAGE) >/dev/null 2>&1 || { \
	  echo "missing image $(IMAGE): run \`make image\` (recipe in Dockerfile)" >&2; exit 1; }

# Том создаётся docker'ом от root, а сборка идёт от хостового uid: без этого
# первый же прогон падает `failed to create directory /cargo/target-tasq: Permission denied`.
cache-init: image-check
	docker run --rm -v $(CARGO_VOL):/cargo $(IMAGE) sh -c 'mkdir -p /cargo/home && chown -R $(UID_GID) /cargo'

fmt: cache-init
	$(RUN) cargo fmt --all -- --check

clippy: cache-init
	$(RUN) cargo clippy --all-targets -- -D warnings

# Политика зависимостей: уязвимости, лицензии, источники, дубли, запрещённые крейты.
# В образе cargo-deny нет, поэтому первый прогон ставит его в общий том (дальше
# команда уже на месте): установка — тем же скриптом, что и шаг CI.
deny: cache-init
	$(RUN) sh -c 'export PATH="$$CARGO_HOME/bin:$$PATH"; command -v cargo-deny >/dev/null 2>&1 || sh scripts/install-cargo-deny.sh; cargo deny check'

# Покрытие. Порог — пол, ниже которого сборка не выпускается, а не цель: он не должен
# мешать добавлять код, но обязан ловить «написали и не проверили».
# Отдельная цель, потому что инструментированная сборка пересобирает всё дерево.
#
# Перед запуском проверяется наличие инструмента в sysroot. Без проверки отсутствие компонента
# выясняется не отказом, а вопросом `Proceed? [Y/n]` от rustup и ожиданием загрузки: на медленном
# канале это выглядит как зависший замер, а в CI — как таймаут джобы. Отказ сразу и с подсказкой
# честнее: причина названа, действие понятно.
COVERAGE_MIN ?= 90
cov: cache-init image-check
	@$(RUN) sh -c 'llvm_cov="$$(rustc --print sysroot)/lib/rustlib/$$(rustc -vV | sed -n "s/^host: //p")/bin/llvm-cov"; \
	  if [ ! -x "$$llvm_cov" ]; then \
	    echo "LLVM_TOOLS_MISSING: нет $$llvm_cov — llvm-tools-preview отсутствует в образе $(IMAGE); пересобери: make image" >&2; \
	    exit 1; \
	  fi' || exit 1
	$(RUN) cargo llvm-cov --all-targets --summary-only --fail-under-lines $(COVERAGE_MIN)

test: cache-init
	$(RUN) cargo test --all-targets

build: cache-init
	$(RUN) cargo build --release

# Бинарь вынимается из тома отдельным контейнером, и кладётся через rename: `cp` поверх
# запущенного файла падает `Text file busy` (а MCP-сервер держит его открытым), rename
# же подменяет запись каталога, и работающий процесс продолжает со старого inode.
install: build
	docker run --rm -v $(CARGO_VOL):/cargo -v $(HOME)/.local/bin:/out $(IMAGE) sh -c \
	  'cp /cargo/target-tasq/release/tasq /out/.tasq.new && chown $(UID_GID) /out/.tasq.new \
	   && chmod 0755 /out/.tasq.new && mv -f /out/.tasq.new /out/tasq'
	$(HOME)/.local/bin/tasq --version

# Порог glibc у готового бинаря — часть контракта артефакта, а не следствие сборки.
# Astra/Debian 11 несут 2.31; сравнение по версии, а не по строке, иначе 2.9 окажется
# «больше» 2.31. Узнавать порог из ошибки загрузчика у пользователя — не проверка.
GLIBC_MAX ?= 2.31
# Негативный контроль в том же шаге: предел, который НИЖЕ реального порога, но ВЫШЕ его при
# строковом сравнении (2.9 «больше» 2.30), обязан быть отвергнут. Без этой половины гейт,
# сравнивающий строки, проходит всегда, и проверка «крышка не превышена» ничего не значит.
glibc-check: build
	$(RUN) sh -c 'set -e; \
	  sh scripts/check-glibc-floor.sh $(TARGET_DIR)/release/tasq $(GLIBC_MAX); \
	  if sh scripts/check-glibc-floor.sh $(TARGET_DIR)/release/tasq 2.9 >/dev/null 2>&1; then \
	    echo "glibc-check: gate accepts 2.9 for a 2.3x binary — versions compare as strings" >&2; exit 1; \
	  else echo "glibc: negative control ok (2.9 rejected)"; fi'

smoke: build
	$(RUN) sh -c 'set -e; \
	  work=$$(mktemp -d); cd $$work; \
	  $(TARGET_DIR)/release/tasq init --key TASQ --name smoke; \
	  $(TARGET_DIR)/release/tasq doctor --check; \
	  $(TARGET_DIR)/release/tasq doctor --json > doctor.json; \
	  cat doctor.json; grep -q "\"ok\": true" doctor.json'

# Гейт бюджета схем MCP: тот же порог, что в CI.
tokens: build
	$(RUN) $(TARGET_DIR)/release/tasq tokens --max 6000

# Мутационный замер. Отдельно от `all`: полный прогон — это минуты на файл, а не секунды.
#
# Настройки — в .cargo/mutants.toml (узкая команда тестов, кэш линтов, копия target);
# `--baseline skip` потому что немутированное дерево проверяет `make test`.
# `-j` по умолчанию 8: каждый воркер копирует каталог сборки, и при большем числе
# воркеров копии начинают конкурировать за диск сильнее, чем выигрывают на CPU.
MUT_JOBS ?= 8
# Параллелизм — по деревьям (scripts/mutants-parallel.sh), а не `-j`: `-j` копирует каталог
# сборки на каждого мутанта, а `--in-place` с `-j` несовместим. Скрипт делит набор на шарды,
# каждому воркеру даёт своё дерево и свой кэш, связанный с базовым жёсткими ссылками.
MUT_WORKERS ?= 8
# Страж: без него отсутствие `cargo-mutants` в томе выясняется не отказом, а отчётом
# «убито 0, выжило 0» — то есть ноль проверки, замаскированный под успешный замер.
# Ровно так и случилось: все восемь воркеров завершались с rc=101 и «no such command: mutants»,
# а итоговая строка выглядела как «мутантов не найдено».
define require_mutants
	@$(RUN) sh -c 'command -v cargo-mutants >/dev/null || { \
	  echo "CARGO_MUTANTS_MISSING: cargo-mutants нет в PATH; он в образе — пересобери: make image" >&2; \
	  exit 1; }' || exit 1
endef
mutants: cache-init
	$(require_mutants)
	$(RUN) bash scripts/mutants-parallel.sh $(MUT_WORKERS)

# Один файл: так сайты разбираются пачками, и замер стоит секунды, а не минуты.
# Для файла, чьи проверки живут в интеграционных наборах, команду можно сузить явно:
#   make mutants-file FILE=src/mcp/tools.rs TEST_ARGS='--test mcp_tools'
#
# `$(TEST_ARGS)` уходит за вторым `--`: это аргументы ТЕСТОВ, а не опции mutator'а. Без разделителя
# `--test` попадает в опции, разбор падает с кодом 1, а итог печатается как «убито 0, выжило 0» —
# ноль проверки, который неотличим от честного нуля.
FILE ?= src/project.rs
TEST_ARGS ?=
mutants-file: cache-init
	$(require_mutants)
	$(RUN) bash scripts/mutants-parallel.sh $(MUT_WORKERS) -- -f $(FILE) $(if $(strip $(TEST_ARGS)),-- $(TEST_ARGS))

# Только изменённое относительно базовой ветки: гейт для повседневной работы.
BASE ?= origin/main
mutants-diff: cache-init
	$(require_mutants)
	$(RUN) sh -c 'git diff $(BASE) > /tmp/mutants.diff && bash scripts/mutants-parallel.sh $(MUT_WORKERS) -- --in-diff /tmp/mutants.diff'

# Справочник в docs/ снимается с бинаря; правка руками ловится гейтом.
docs: build
	$(RUN) sh -c '$(TARGET_DIR)/release/tasq docs && $(TARGET_DIR)/release/tasq docs --check'

act:
	act push -j check

clean:
	docker run --rm -v $(CARGO_VOL):/cargo $(IMAGE) rm -rf $(TARGET_DIR)
