//! Session store: SQLite persistence and the typed data model (doc 09).

pub mod db;
pub mod model;

pub use db::SessionStore;
