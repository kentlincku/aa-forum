//! 領域核心：事件匯流排、儲存與遷移、權限、設定、supervisor、背景工作、功能模組註冊。
//!
//! 核心不認識任何具體功能或 agent；功能以 [`Feature`] 掛上。

pub mod config;
pub mod error;
pub mod events;
pub mod feature;
pub mod jobs;
pub mod perms;
pub mod storage;
pub mod supervisor;

pub use error::{Error, Result};
pub use feature::{Core, Feature, Registry};
