#!/bin/sh
# Полный замер по ub-mode: покрытие -> CRAP -> дубликаты. Одна серия, один лог.
# --no-cfg-coverage: иначе зависимость с cfg_attr(coverage, feature(..)) роняет сборку на стабильном тулчейне.
# mkdir -p до вызова: cargo llvm-cov не создаёт родительский каталог для отчёта.
set -u
mkdir -p /w/target/coverage

echo "=== 1. покрытие (lcov + сводка)"
cargo llvm-cov --all-targets --no-cfg-coverage \
  --lcov --output-path /w/target/coverage/lcov.info
echo "COV_EXIT=$?"

echo "=== 1b. сводка по файлам"
cargo llvm-cov report --summary-only 2>&1 | tail -40
echo "SUM_EXIT=$?"

echo "=== 2. CRAP (lcov от того же прогона)"
cargo crap --lcov /w/target/coverage/lcov.info --workspace 2>&1 | tail -60
echo "CRAP_EXIT=$?"

echo "=== 3. дубликаты"
cargo dupes 2>&1 | tail -40
echo "DUPES_EXIT=$?"
