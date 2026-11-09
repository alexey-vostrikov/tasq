#!/bin/sh
# Порог glibc готового бинаря — один скрипт для `make glibc-check` и шага CI.
#
# Зачем: база образа задаёт минимальную glibc артефакта, и узнать о ней из ошибки
# загрузчика у пользователя («GLIBC_2.34 not found» на Astra/Debian 11) — не проверка.
# Порог берётся из таблицы динамических символов бинаря, а версии сравниваются
# по номеру: строковое сравнение поставило бы 2.9 выше 2.31.
#
#   scripts/check-glibc-floor.sh <бинарь> [предел]
#
# Код выхода 1 — порог превышен или символов GLIBC нет вовсе (статическая сборка
# тоже означает, что проверять нечего и гейт врёт).
set -eu

bin="${1:?usage: check-glibc-floor.sh <binary> [max]}"
limit="${2:-2.31}"

[ -f "$bin" ] || { echo "glibc-check: no binary at $bin" >&2; exit 1; }

max="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -V -u | tail -1)"
[ -n "$max" ] || { echo "glibc-check: no GLIBC symbols found in $bin" >&2; exit 1; }

highest="$(printf '%s\n%s\n' "$limit" "$max" | sort -V | tail -1)"
if [ "$highest" != "$limit" ]; then
  echo "glibc-check: $bin needs GLIBC_$max, limit is $limit" >&2
  exit 1
fi

echo "glibc: $max <= $limit ok"
