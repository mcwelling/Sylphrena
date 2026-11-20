/// `show_image` tool — renders an image file from the container workspace into the terminal.
///
/// Tries timg first (always outputs correct Kitty protocol in non-TTY contexts),
/// falls back to chafa if timg is unavailable. Both renderers are invoked via
/// `podman exec` with `TERM=xterm-kitty` to ensure correct protocol selection.
use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use crate::config::{container_name, AppConfig};
use crate::shared::SharedState;
use crate::tui::{image_proto, terminal};

/// Render an image to the terminal. Returns `(status_message, is_error)`.
pub fn show_image(
    input:  &serde_json::Value,
    config: &AppConfig,
    state:  &Arc<SharedState>,
    rows:   u16,
    cols:   u16,
) -> (String, bool) {
    let _ = config; // currently unused; kept for API consistency with other tool fns

    let rel_path = input["path"].as_str().unwrap_or("").trim();
    if rel_path.is_empty() {
        return ("[Tool error: show_image requires 'path']".to_string(), true);
    }

    // Normalise: accept both workspace-relative and /workspace/-prefixed paths
    let container_path = format!("/workspace/{}", rel_path.trim_start_matches('/'));

    // Verify the file exists inside the container before invoking a renderer
    let check = Command::new("podman")
        .args(["exec", container_name(), "test", "-f", &container_path])
        .status();
    if !check.map(|s| s.success()).unwrap_or(false) {
        return (format!("[Tool error: file not found: {}]", rel_path), true);
    }

    // Detect terminal image protocol (respects SYL_IMAGE override)
    let protocol  = image_proto::detect_image_protocol();
    let (img_cols, img_rows) = image_size_chars(cols, rows);

    // Try timg first (reliable Kitty output); fall back to chafa
    let bytes = render_timg(&container_path, img_cols, img_rows)
        .or_else(|| render_chafa(&container_path, &protocol, img_cols, img_rows));

    let bytes = match bytes {
        Some(b) if !b.is_empty() => b,
        _ => return (format!("[show_image: could not render {}]", rel_path), true),
    };

    // Wrap in tmux DCS passthrough if running inside tmux
    let bytes = if image_proto::is_tmux() {
        image_proto::wrap_for_tmux(&bytes)
    } else {
        bytes
    };

    // Write to terminal: move into scroll region, emit raw bytes, advance cursor
    {
        let mut out = state.stdout.lock().unwrap();
        let _ = terminal::move_to_scroll(&mut *out, rows);
        let _ = terminal::write_raw(&mut *out, &bytes);
        let _ = out.write_all(b"\r\n");
    }

    let caption = input["caption"].as_str().unwrap_or("");
    let msg = if caption.is_empty() {
        format!("[Image: {}]", rel_path)
    } else {
        format!("[Image: {} \u{2014} {}]", rel_path, caption)
    };
    (msg, false)
}

/// Compute image display size in character cells.
/// Reserves ~6 rows for bars + prompt + margin; uses 60% of remaining height.
fn image_size_chars(cols: u16, rows: u16) -> (u16, u16) {
    let h = (rows.saturating_sub(6) * 60 / 100).max(4);
    (cols.saturating_sub(2), h)
}

/// Run a renderer binary inside the container.
///
/// `args[0]` must be the binary name (e.g. `"timg"` or `"chafa"`);
/// subsequent elements are flags. `container_path` is appended last.
fn renderer_cmd(container_path: &str, args: &[&str]) -> Option<Vec<u8>> {
    let result = Command::new("podman")
        .args(["exec", "--env", "TERM=xterm-kitty", "--env", "COLORTERM=truecolor",
               container_name()])
        .args(args)
        .arg(container_path)
        .output()
        .ok()?;
    if result.status.success() && !result.stdout.is_empty() {
        Some(result.stdout)
    } else {
        None
    }
}

fn render_timg(path: &str, cols: u16, rows: u16) -> Option<Vec<u8>> {
    renderer_cmd(path, &["timg", "--compress", &format!("-g{}x{}", cols, rows)])
}

fn render_chafa(path: &str, protocol: &image_proto::ImageProtocol, cols: u16, rows: u16) -> Option<Vec<u8>> {
    let fmt_flag = match protocol {
        image_proto::ImageProtocol::Kitty   => "--format=kitty",
        image_proto::ImageProtocol::Iterm2  => "--format=iterm",
        image_proto::ImageProtocol::Sixel   => "--format=sixel",
        image_proto::ImageProtocol::Symbols => "--format=symbols",
    };
    renderer_cmd(path, &["chafa", fmt_flag, &format!("--size={}x{}", cols, rows)])
}
