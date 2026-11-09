#!/bin/sh
# Установка cargo-deny одним и тем же способом в локальный образ и в CI.
#
# Один скрипт вместо двух копий логики: установка в Dockerfile и установка шагом
# в GitHub Actions не должны разъезжаться, иначе «проходит локально» перестаёт
# что-либо значить.
#
#   scripts/install-cargo-deny.sh [версия]
#
# Бинарь кладётся в /usr/local/cargo/bin — этот каталог уже в PATH у образов rust.
set -eu

version="${1:-0.20.2}"

case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-musl ;;
  aarch64) target=aarch64-unknown-linux-musl ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

base="https://github.com/EmbarkStudios/cargo-deny/releases/download/${version}/cargo-deny-${version}-${target}.tar.gz"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

curl -fsSL "$base" -o "$work/cargo-deny.tar.gz"
curl -fsSL "$base.sha256" -o "$work/cargo-deny.sha256"

# В файле бывает только хеш или «хеш  имя файла» — берём первое поле.
echo "$(awk '{print $1}' "$work/cargo-deny.sha256")  $work/cargo-deny.tar.gz" | sha256sum -c -

tar -xzf "$work/cargo-deny.tar.gz" -C "$work"
# Каталог берётся из CARGO_HOME: в образе с общим томом (CARGO_HOME=/cargo) писать в
# /usr/local/cargo нельзя — процесс идёт от хостового uid, и установка падает по правам.
bin_dir="${CARGO_HOME:-/usr/local/cargo}/bin"
mkdir -p "$bin_dir"
install -m 0755 "$work/cargo-deny-${version}-${target}/cargo-deny" "$bin_dir/cargo-deny"

"$bin_dir/cargo-deny" --version
