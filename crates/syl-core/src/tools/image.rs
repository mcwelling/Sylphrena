//! `show_image` tool: checks that an image exists in the container.
//!
//! Drawing it is the client's job. The TUI renders the file when it sees the
//! successful `ToolCallFinished` event; headless clients just report it.
use syl_container::Container;

/// The in-container path of an image: relative paths are under the
/// workspace; absolute ones are taken as they are.
pub fn container_path(container: &Container, rel_path: &str) -> String {
    container.resolve(rel_path)
}

/// Run `show_image`. Returns `(status_message, is_error)`.
pub fn show_image(input: &serde_json::Value, container: &Container) -> (String, bool) {
    let rel_path = input["path"].as_str().unwrap_or("").trim();
    if rel_path.is_empty() {
        return ("[Tool error: show_image requires 'path']".to_string(), true);
    }
    let exists = container
        .exec_program(&[], &["test", "-f", &container_path(container, rel_path)])
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !exists {
        return (format!("[Tool error: file not found: {rel_path}]"), true);
    }
    let caption = input["caption"].as_str().unwrap_or("");
    let msg = if caption.is_empty() {
        format!("[Image: {rel_path}]")
    } else {
        format!("[Image: {rel_path} \u{2014} {caption}]")
    };
    (msg, false)
}
