//! Syl's agent core: the loop, tools, sessions, compaction and the model
//! client. It has no terminal dependency; clients drive it through
//! `agent::spawn`, `AgentCommand` and `AgentEvent`.
pub mod agent;
pub mod attachments;
pub mod checkpoint;
pub mod api;
pub mod config;
pub mod control;
pub mod logging;
pub mod model;
pub mod permissions;
pub mod pricing;
pub mod session;
pub mod shared;
pub mod system_prompt;
pub mod tools;
pub mod util;
pub mod workspace;

pub use agent::{AgentCommand, AgentEvent, AgentHandle, SessionStart, TurnEnd};
pub use syl_container as container;
