//! Draws an image from the container workspace into the terminal, after the
//! core's `show_image` tool has confirmed the file exists.
//!
//! Tries timg first (reliable Kitty output in non-TTY contexts) and falls
//! back to chafa. Both run in the container with `TERM=xterm-kitty`.
use std::io::Write;

use syl_container::Container;
use syl_core::tools::image::container_path;

use crate::tui::state::TuiState;
use crate::tui::{image_proto, terminal};

/// Render `rel_path`. Returns false if neither renderer produced output.
pub fn render_image(rel_path: &str, container: &Container, tui: &TuiState, rows: u16, cols: u16) -> bool {
    let path = container_path(rel_path);
    let protocol = image_proto::detect_image_protocol();
    let (img_cols, img_rows) = image_size_chars(cols, rows);

    let bytes = render_timg(container, &path, img_cols, img_rows)
        .or_else(|| render_chafa(container, &path, &protocol, img_cols, img_rows));
    let Some(bytes) = bytes.filter(|b| !b.is_empty()) else {
        return false;
    };
    let bytes = if image_proto::is_tmux() { image_proto::wrap_for_tmux(&bytes) } else { bytes };

    let mut out = tui.stdout.lock().unwrap();
    let _ = terminal::move_to_scroll(&mut *out, rows);
    let _ = terminal::write_raw(&mut *out, &bytes);
    let _ = out.write_all(b"\r\n");
    true
}

/// Image size in character cells: reserve ~6 rows for bars, prompt and
/// margin, then use 60% of the remaining height.
fn image_size_chars(cols: u16, rows: u16) -> (u16, u16) {
    let h = (rows.saturating_sub(6) * 60 / 100).max(4);
    (cols.saturating_sub(2), h)
}

fn renderer(container: &Container, argv: &[&str]) -> Option<Vec<u8>> {
    let out = container
        .exec_program(&[("TERM", "xterm-kitty"), ("COLORTERM", "truecolor")], argv)
        .ok()?;
    (out.status.success() && !out.stdout.is_empty()).then_some(out.stdout)
}

fn render_timg(container: &Container, path: &str, cols: u16, rows: u16) -> Option<Vec<u8>> {
    renderer(container, &["timg", "--compress", &format!("-g{cols}x{rows}"), path])
}

fn render_chafa(
    container: &Container,
    path: &str,
    protocol: &image_proto::ImageProtocol,
    cols: u16,
    rows: u16,
) -> Option<Vec<u8>> {
    let fmt_flag = match protocol {
        image_proto::ImageProtocol::Kitty => "--format=kitty",
        image_proto::ImageProtocol::Iterm2 => "--format=iterm",
        image_proto::ImageProtocol::Sixel => "--format=sixel",
        image_proto::ImageProtocol::Symbols => "--format=symbols",
    };
    renderer(container, &["chafa", fmt_flag, &format!("--size={cols}x{rows}"), path])
}
