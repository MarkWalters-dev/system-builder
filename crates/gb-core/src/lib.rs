//! Core build engine: config, history, progress parsing, execution, queue.

pub mod config;
pub mod discover;
pub mod history;
pub mod manager;
pub mod progress;
pub mod resolve;
pub mod runner;

pub use config::Config;
pub use history::HistoryStore;
pub use manager::{Manager, ManagerError};
