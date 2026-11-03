//! 執行層：Driver 介面、ACP client、command／manual driver、dispatcher。
//!
//! 只走 ACP：互動式 agent 一律經 ACP；command 是無模型測試路徑；manual 代表人。

pub mod accounts;
pub mod acp;
pub mod catalog;
pub mod command;
pub mod dispatcher;
pub mod driver;
pub mod headless;
pub mod hooks;
pub mod manual;
pub mod process;
pub mod ratelimit;
pub mod testing;

pub use driver::{
    AgentStatus, ContextUsage, Driver, DriverEvent, DriverSpec, PermissionDecision, SteerOutcome,
    TurnResult,
};
pub mod continuity;
pub mod sanitize;
