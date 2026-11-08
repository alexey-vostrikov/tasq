# Сборка ведётся на той же базе, что и CI: rust:1.99.1-bookworm.
# Официальный образ не несёт rustfmt и clippy, поэтому они ставятся в образ один раз,
# а не в каждой команде (в CI то же делается шагом rustup component add).
FROM rust:1.99.1-bookworm

RUN rustup component add rustfmt clippy
