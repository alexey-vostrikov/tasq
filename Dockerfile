# Сборка ведётся на той же базе, что и CI: rust:1.99.1-bookworm.
# Официальный образ не несёт rustfmt, clippy и cargo-deny, поэтому они ставятся в образ
# один раз, а не в каждой команде. Установка cargo-deny — тем же скриптом, что и в CI,
# чтобы локальные гейты и GitHub Actions не разъезжались.
FROM rust:1.99.1-bookworm

RUN rustup component add rustfmt clippy

COPY scripts/install-cargo-deny.sh /usr/local/bin/install-cargo-deny
RUN sh /usr/local/bin/install-cargo-deny
