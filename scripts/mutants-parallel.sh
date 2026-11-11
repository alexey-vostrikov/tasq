#!/bin/bash
# Мутационный замер в несколько воркеров — «как должно быть».
#
# Почему так, а не `cargo mutants -j N`:
#
#   * `--in-place` в одном дереве даёт максимально дешёвый шаг (пересобирается только наш
#     крейт), но он ПРОТИВОРЕЧИТ `--jobs`: мутировать дерево из нескольких процессов нельзя.
#   * `-j N` без `--in-place` копирует дерево воркера вместе с каталогом сборки. Наш target
#     большой (единицы гигабайт), и копирование съедает выигрыш: замер на 5 мутантах — 126 с
#     против 102 с последовательно.
#
# Поэтому параллелизм делается снаружи: каждому воркеру — своё дерево исходников (копия,
# мелкая) и свой каталог сборки, полученный ЖЁСТКИМИ ССЫЛКАМИ (`cp -al`, 0.1 с на гигабайты).
# Внутри воркера — `--in-place`, то есть без копирований вообще; между воркерами — `--shard i/N`.
#
# Требования: каталог исходников и кэш сборки на одной файловой системе (иначе `cp -al`
# откажется: «Неверная ссылка между устройствами»).
#
# Использование:
#   scripts/mutants-parallel.sh [WORKERS] [-- cargo-mutants args…]
#
# Примеры:
#   scripts/mutants-parallel.sh 8                       весь набор
#   scripts/mutants-parallel.sh 8 -- -f src/project.rs  один файл
#   scripts/mutants-parallel.sh 4 -- --in-diff /tmp/x.diff
#   scripts/mutants-parallel.sh 8 -- -f src/status.rs -- --test status
#
# Второй `--` делит вызов на опции cargo-mutants и аргументы ТЕСТОВ. Без него аргументы тестов
# уходят в опции и разбор падает: `--test` не опция cargo-mutants, и воркеры завершаются кодом 1,
# а итог печатается как «убито 0, выжило 0» — то есть ноль проверки, неотличимый от честного нуля.
set -uo pipefail

WORKERS="${1:-4}"
shift || true
[ "${1:-}" = "--" ] && shift

# Разделение: до второго `--` — опции mutator'а, после — аргументы тестов. Массивы в bash, а не
# в sh: скрипт запускается через `bash`, и `${arr[@]}` в dash дал бы синтаксическую ошибку.
MUT_OPTS=()
TEST_ARGS=()
seen_sep=0
for arg in "$@"; do
  if [ "$seen_sep" -eq 0 ] && [ "$arg" = "--" ]; then
    seen_sep=1
    continue
  fi
  if [ "$seen_sep" -eq 1 ]; then
    TEST_ARGS+=("$arg")
  else
    MUT_OPTS+=("$arg")
  fi
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WT="$ROOT/.mutwt"
BASE_TARGET="$ROOT/target/mut"

echo "== воркеров: $WORKERS; базовый кэш: $BASE_TARGET"

# Базовый кэш собирается один раз: без него каждый воркер начнёт с нуля.
if [ ! -d "$BASE_TARGET/debug" ]; then
  echo "== прогреваю базовый кэш (cargo test --lib --no-run)"
  ( cd "$ROOT" && CARGO_TARGET_DIR="$BASE_TARGET" cargo test --lib --no-run --quiet ) || exit 1
fi

mkdir -p "$WT"

# Память: каждый воркер со ВСЕМ набором тестов поднимает десятки процессов и легко упирается
# в память — замер с 6 воркерами и `--all-targets` был убит сигналом 9 (код 137) через 405 с.
# Поэтому по умолчанию 4 воркера, а полный набор гоняется на 2: узкая команда тестов
# (`TEST_ARGS='--test mcp_tools'`) позволяет и 8.
#
# Раньше здесь было предупреждение и продолжение: прогон печатал «может быть убит по памяти» и
# через семь минут действительно получал 137 — то есть отчёт «убито 0, выжило 0» с безвестной
# причиной. Предупреждение, после которого случается ровно то, о чём предупредили, — это не
# проверка, а извинение: отказ сразу называет и число, и выход.
if [ "$WORKERS" -gt 4 ] && [ -z "${MUT_MEMORY_OK:-}" ]; then
  echo "TOO_MANY_WORKERS: $WORKERS воркеров × сборка каждого не укладываются в память" >&2
  echo "hint: возьми 2–4 воркера (по умолчанию 4) или подтверди осознанно: MUT_MEMORY_OK=1" >&2
  echo "      (замер: 6 воркеров × --all-targets = код 137 через 405 с)" >&2
  exit 1
fi

prepare() {
  local i="$1" dir="$WT/w$i"
  rm -rf "$dir"
  mkdir -p "$dir"
  # Исходники — обычной копией: их будут мутировать, жёсткие ссылки вернули бы подмену назад.
  # Исключаем каталог сборки, историю и рабочие каталоги замеров.
  tar -C "$ROOT" -cf - \
      --exclude=./target --exclude=./.mutwt --exclude=./.git --exclude=./mutants.out \
      --exclude=./.install . | tar -C "$dir" -xf -
  # Кэш сборки — жёсткими ссылками: 0.1 с вместо копирования гигабайтов.
  mkdir -p "$dir/target"
  cp -al "$BASE_TARGET" "$dir/target/mut" || {
    echo "!! w$i: cp -al не сработал (разные файловые системы?)" >&2
    return 1
  }
  # Артефакты НАШЕГО крейта не разделяем: воркер перезапишет их своим мутантом, а по
  # жёсткой ссылке это испортило бы базовый кэш и соседей.
  rm -rf "$dir/target/mut/debug/incremental"
  find "$dir/target/mut/debug/deps" -maxdepth 1 -name 'tasq-*' -delete 2>/dev/null
  rm -f "$dir/target/mut/debug/libtasq"* "$dir/target/mut/debug/tasq" 2>/dev/null
}

pids=()
# Остатки прошлых прогонов: убитый замер оставляет деревья воркеров, и они копятся — 8 воркеров
# это 23 ГБ, обнаружено на этой машине. Удаляются деревья с номерами больше текущего их числа:
# свои всё равно пересоздаются в `prepare`.
for stale in "$WT"/w*; do
  [ -d "$stale" ] || continue
  idx=${stale##*/w}
  case "$idx" in
    ''|*[!0-9]*) continue ;;
  esac
  [ "$idx" -gt "$WORKERS" ] && rm -rf "$stale"
done

for i in $(seq 1 "$WORKERS"); do
  prepare "$i" || exit 1
  (
    cd "$WT/w$i" || exit 1
    CARGO_TARGET_DIR="$WT/w$i/target/mut" \
      cargo mutants --in-place --baseline skip --timeout "${MUT_TIMEOUT:-120}" \
        --shard "$((i - 1))/$WORKERS" "${MUT_OPTS[@]}" \
        ${TEST_ARGS:+-- "${TEST_ARGS[@]}"} \
      > "log.txt" 2>&1
    echo $? > "rc.txt"
  ) &
  pids+=($!)
done

for pid in "${pids[@]}"; do wait "$pid"; done

# Нумерация шарда НУЛЕВАЯ: `--shard 3/3` отклоняется («shard k must be less than n»),
# поэтому воркер i идёт под номером i-1. Перепутать легко: промах даёт «Found 0 mutants»
# у одного воркера и молча пропущенный шард, а не ошибку.
#
# Итог: складываем шарды. Коды возврата cargo-mutants: 0 — все убиты, 2 — есть выжившие,
# 4 — есть таймауты, 3 — ошибка, 1 — внутренняя ошибка. Печатаем все ненулевые.
echo
echo "== итог по шардам"
total=0; caught=0; missed=0; unviable=0; timeout=0
for i in $(seq 1 "$WORKERS"); do
  dir="$WT/w$i"
  rc="$(cat "$dir/rc.txt" 2>/dev/null || echo '?')"
  line="$(grep -E 'mutants tested in' "$dir/log.txt" 2>/dev/null | tail -1)"
  echo "-- w$i (rc=$rc): ${line:-нет строки итога}"
  [ -s "$dir/mutants.out/caught.txt" ]   && caught=$((caught + $(grep -c . "$dir/mutants.out/caught.txt")))
  [ -s "$dir/mutants.out/missed.txt" ]   && missed=$((missed + $(grep -c . "$dir/mutants.out/missed.txt")))
  [ -s "$dir/mutants.out/timeout.txt" ]  && timeout=$((timeout + $(grep -c . "$dir/mutants.out/timeout.txt")))
  [ -s "$dir/mutants.out/unviable.txt" ] && unviable=$((unviable + $(grep -c . "$dir/mutants.out/unviable.txt")))
  if [ "$rc" != "0" ] && [ "$rc" != "2" ] && [ "$rc" != "4" ]; then
    echo "!! w$i завершился с кодом $rc — смотри $dir/log.txt" >&2
  fi
done

# Деревья сборки воркеров удаляются по завершении — на любом пути, включая отказ порога: `trap`
# на EXIT, а не `rm` перед каждым `exit`. Логи и `mutants.out` остаются, чтобы выжившие и
# таймауты можно было разобрать; `MUT_KEEP=1` сохраняет и сборку (нужна, если разбирать
# отдельного мутанта руками). Без этого каждый прогон оставлял 9–23 ГБ.
cleanup_workers() {
  [ -n "${MUT_KEEP:-}" ] && return 0
  for i in $(seq 1 "$WORKERS"); do
    rm -rf "$WT/w$i/target" 2>/dev/null || true
  done
}
trap cleanup_workers EXIT

echo "== всего: убито $caught, выжило $missed, таймаут $timeout, нежизнеспособно $unviable"

# Пороги. Без них отчёт «убито 56, выжило 0» — это информация, а не гейт: прогон с 40 выжившими
# тоже завершился бы кодом 0 и ничего бы не остановил. Два числа, потому что они отвечают на
# разные вопросы:
#
#   score            = убито / (убито + выжило + таймаут)
#                      «из тех мутантов, которых вообще можно было судить, сколько тесты поймали»
#   unverified share = (выжило + таймаут + нежизнеспособно) / всего
#                      «какая доля мутаций осталась без проверки»: выжившие и таймауты — тесты не
#                      справились, нежизнеспособные — мутация не собралась, то есть и этот путь
#                      никем не проверен.
#
# Порог по score считается БЕЗ нежизнеспособных намеренно: мутация, которая не компилируется, не
# свойство тестов, и подмешивать её в знаменатель значило бы поощрять непроверяемый код.
#
# Значения — из первого успешного замера (src/status.rs: 63 мутанта, 56 убито, 0 выжило,
# 7 нежизнеспособно → score 100 %, unverified 11 %), с запасом вниз: порог, который красный
# с первого прогона на историческом долге, отключают, и он перестаёт что-либо значить.
MUT_MIN_SCORE="${MUT_MIN_SCORE:-90}"
MUT_MAX_UNVERIFIED="${MUT_MAX_UNVERIFIED:-20}"

viable=$((caught + missed + timeout))
total=$((viable + unviable))
if [ "$viable" -gt 0 ]; then
  score=$((caught * 100 / viable))
  unverified=$(( (missed + timeout + unviable) * 100 / total ))
  echo "== score: $score % (порог $MUT_MIN_SCORE %), без проверки: $unverified % (порог $MUT_MAX_UNVERIFIED %)"
  gate_failed=0
  if [ "$score" -lt "$MUT_MIN_SCORE" ]; then
    echo "MUTATION_SCORE_BELOW: $score % < $MUT_MIN_SCORE %: тесты пропускают мутации" >&2
    gate_failed=1
  fi
  if [ "$unverified" -gt "$MUT_MAX_UNVERIFIED" ]; then
    echo "MUTATION_UNVERIFIED_ABOVE: $unverified % > $MUT_MAX_UNVERIFIED %: мутации остаются без проверки" >&2
    gate_failed=1
  fi
  [ "$gate_failed" -eq 1 ] && exit 3
else
  # Ноль судимых мутантов — это не «всё отлично», а «замер ничего не проверил»: выбранный файл
  # не содержит мутируемых мест или мутатор их все отсёк. Молча вернуть 0 значило бы выдать
  # отсутствие проверки за успех.
  echo "MUTATION_NOTHING_TESTED: ни один мутант не был проверен ($total нежизнеспособно(ых))" >&2
  exit 3
fi

if [ "$missed" -gt 0 ]; then
  echo "== выжившие:"
  for i in $(seq 1 "$WORKERS"); do
    f="$WT/w$i/mutants.out/missed.txt"
    [ -s "$f" ] && sed "s/^/w$i /" "$f"
  done
  exit 2
fi
[ "$timeout" -gt 0 ] && exit 4
exit 0
