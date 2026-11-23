pub mod bash;
pub mod dispatch;
pub mod files;
pub mod image;
pub mod memory;
pub mod workflow;

use std::path::Path;

use syl_container::Container;
use tokio_util::sync::CancellationToken;

use crate::config::AppConfig;
use crate::model::ModelAdapter;

/// What a tool call needs from the session.
pub struct ToolContext<'a> {
    pub config: &'a AppConfig,
    pub model: &'a dyn ModelAdapter,
    pub container: &'a Container,
    pub memory_dir: &'a Path,
    pub workflow_dir: &'a Path,
}

/// Run one tool call. Returns `(output, is_error)`.
pub async fn execute(
    name: &str,
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
    cancel: CancellationToken,
) -> (String, bool) {
    let str_field = |key: &str| input[key].as_str().unwrap_or("").to_string();
    match name {
        "run_bash" => bash::run_bash(input, ctx.config, ctx.container, cancel).await,
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
        "read_file" => files::read_file(input, ctx.container),
        "dispatch_agent" => {
            dispatch::dispatch_agent(input, ctx.config, ctx.model, ctx.container, cancel).await
        }
        "show_image" => image::show_image(input, ctx.container),
        other => (format!("[Tool error: unknown tool `{other}`]"), true),
    }
}
