//! tasq — локальный трекер задач для ИИ-агентов.
//!
//! Библиотека содержит всю логику; `main.rs` — тонкая обёртка над CLI,
//! чтобы модули были доступны интеграционным тестам.

#![forbid(unsafe_code)]

pub mod activity;
pub mod budget;
pub mod burnup;
pub mod comment;
pub mod connect;
pub mod db;
pub mod docs;
pub mod edge;
pub mod error;
pub mod history;
pub mod lease;
pub mod mcp;
pub mod node;
pub mod note;
pub mod overview;
pub mod project;
pub mod resource;
pub mod status;
pub mod template;
pub mod transfer;

pub use error::{Error, Result};

/// Версия бинаря (из `Cargo.toml`).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
