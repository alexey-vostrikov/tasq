//! tasq — локальный трекер задач для ИИ-агентов.
//!
//! Библиотека содержит всю логику; `main.rs` — тонкая обёртка над CLI,
//! чтобы модули были доступны интеграционным тестам.

#![forbid(unsafe_code)]

pub mod db;
pub mod error;

pub use error::{Error, Result};

/// Версия бинаря (из `Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
