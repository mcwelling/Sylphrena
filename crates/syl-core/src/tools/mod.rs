pub mod bash;
pub mod dispatch;
pub mod fs;
pub mod image;
pub mod memory;
pub mod workflow;

use std::path::Path;

use syl_container::Container;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::model::ModelAdapter;
use crate::util::SpillDir;

/// What a tool call needs from the session.
pub struct ToolContext<'a> {
    pub config: &'a AppConfig,
    pub model: &'a dyn ModelAdapter,
    pub container: &'a Container,
    pub memory_dir: &'a Path,
    pub workflow_dir: &'a Path,
    /// Where `run_bash` saves output too long to show in full.
    pub spill: Option<&'a SpillDir>,
}

/// Tools that only read: a run of these in one turn is executed
/// concurrently. Everything else runs one at a time, in order.
pub fn is_read_only(name: &str) -> bool {
    matches!(name, "read_file" | "grep" | "glob" | "ls" | "memory_read")
}

/// Run one tool call. Returns `(output, is_error)`.
pub async fn execute(
    name: &str,
    call_id: &str,
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
    cancel: CancellationToken,
) -> (String, bool) {
    let str_field = |key: &str| input[key].as_str().unwrap_or("").to_string();
    match name {
        "run_bash" => bash::run_bash(input, ctx.config, ctx.container, ctx.spill, call_id, cancel).await,
        "memory_read" => (memory::memory_read(ctx.memory_dir, &str_field("key")), false),
        "memory_write" => (
            memory::memory_write(ctx.memory_dir, &str_field("key"), &str_field("content")),
            false,
        ),
        "workflow_write" => (
            workflow::workflow_write(ctx.workflow_dir, &str_field("name"), &str_field("content")),
            false,
        ),
        "workflow_delete" => (workflow::workflow_delete(ctx.workflow_dir, &str_field("name")), false),
        "read_file" => fs::read_file(input, ctx.config, ctx.container, &cancel).await,
        "edit_file" => fs::edit_file(input, ctx.config, ctx.container, &cancel).await,
        "write_file" => fs::write_file(input, ctx.config, ctx.container, &cancel).await,
        "grep" => fs::grep(input, ctx.config, ctx.container, &cancel).await,
        "glob" => fs::glob(input, ctx.config, ctx.container, &cancel).await,
        "ls" => fs::ls(input, ctx.config, ctx.container, &cancel).await,
        "dispatch_agent" => {
            dispatch::dispatch_agent(input, ctx.config, ctx.model, ctx.container, cancel).await
        }
        "show_image" => image::show_image(input, ctx.container),
        other => (format!("[Tool error: unknown tool `{other}`]"), true),
    }
}
