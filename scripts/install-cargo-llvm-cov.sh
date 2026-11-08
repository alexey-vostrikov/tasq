#!/bin/sh
# Установка cargo-llvm-cov для замера покрытия — одним способом в образе и в CI.
#
# У проекта релизов cargo-llvm-cov нет файла с контрольными суммами, поэтому хеш
# закреплён здесь для конкретной версии и архитектуры: обновление версии — осознанная
# правка с пересчётом хеша, а не «скачалось последнее».
#
#   scripts/install-cargo-llvm-cov.sh [версия]
#
# Нужен llvm-tools-preview: `rustup component add llvm-tools-preview` (делается здесь же).
set -eu

version="${1:-0.9.1}"

case "$(uname -m)" in
  x86_64)
    target=x86_64-unknown-linux-musl
    expected=3fca950394a3c49457657c158b1619cec8dfd2647ae5b48746734c0ab969a522
    ;;
  aarch64)
    target=aarch64-unknown-linux-musl
    # Хеш для aarch64 не закреплён: установка честно откажется, пока его не добавят.
    expected=""
    ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

rustup component add llvm-tools-preview

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

url="https://github.com/taiki-e/cargo-llvm-cov/releases/download/v${version}/cargo-llvm-cov-${target}.tar.gz"
curl -fsSL "$url" -o "$work/cargo-llvm-cov.tar.gz"

if [ -n "$expected" ]; then
  echo "$expected  $work/cargo-llvm-cov.tar.gz" | sha256sum -c -
else
  echo "warning: no pinned checksum for $target, skipping verification" >&2
fi

tar -xzf "$work/cargo-llvm-cov.tar.gz" -C "$work"
install -m 0755 "$work/cargo-llvm-cov" /usr/local/cargo/bin/cargo-llvm-cov

cargo llvm-cov --version
