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
set -uo pipefail

WORKERS="${1:-4}"
shift || true
[ "${1:-}" = "--" ] && shift

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
if [ "$WORKERS" -gt 4 ] && [ -z "${MUT_MEMORY_OK:-}" ]; then
  echo "!! $WORKERS воркеров: если тесты гоняются полным набором, прогон может быть убит по памяти" >&2
  echo "!! (проверено: 6 воркеров × --all-targets = код 137). Узкие тесты — MUT_MEMORY_OK=1." >&2
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
for i in $(seq 1 "$WORKERS"); do
  prepare "$i" || exit 1
  (
    cd "$WT/w$i" || exit 1
    CARGO_TARGET_DIR="$WT/w$i/target/mut" \
      cargo mutants --in-place --baseline skip --timeout "${MUT_TIMEOUT:-120}" \
        --shard "$((i - 1))/$WORKERS" "$@" \
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

echo "== всего: убито $caught, выжило $missed, таймаут $timeout, нежизнеспособно $unviable"

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
