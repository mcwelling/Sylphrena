//! Interactive arrow-key model picker rendered in the scroll area.
//!
//! # Terminal mechanics
//!
//! The picker prints into the scroll region (rows 2 .. rows-3 in 1-indexed
//! terminal coordinates).  On the initial call it moves the cursor to the last
//! row of the scroll region and emits one `\r\n` per picker line; DECSTBM
//! keeps the cursor at the scroll-region bottom while scrolling old content
//! upward, so the picker appears at the bottom of the visible area.
//!
//! For in-place redraws (key-navigation), `\x1b[{N}A` moves the cursor back
//! to the top of the picker and `\x1b[J` (Erase to End of Screen within the
//! scroll region) clears the old content before reprinting.
//!
//! # Usage
//!
//! Call from a synchronous context (or via `tokio::task::block_in_place`):
//!
//! ```ignore
//! let choice = tokio::task::block_in_place(|| {
//!     picker::run_picker(&items, &current_model, &state.stdout, rows)
//! });
//! ```
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
};
use std::io::Write;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Data type
// ---------------------------------------------------------------------------

/// One selectable entry in the picker.
pub struct ModelOption {
    /// The model ID sent to the API backend (e.g. `"claude-opus-4-6-20250514"`
    /// for Anthropic, `"anthropic.claude-opus-4-6-20250514-v1:0"` for Bedrock).
    pub id: String,
    /// Primary label shown in the left column (e.g. `"Claude Opus 4.6"`).
    /// For static fallback entries this is the same as `id`.
    pub display: String,
    /// Secondary info shown dimmed in the right column.
    /// For dynamic Anthropic/Bedrock entries this is typically the raw model
    /// ID when it differs from `display`; for static fallback it's the
    /// human description.
    pub description: String,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Lines rendered per picker invocation:
///   1 header + 1 blank + N items + 1 trailing blank  =  N + 3
fn total_lines(n: usize) -> usize {
    n + 3
}

/// Write the complete picker UI to `out`.  Each line ends with `\r\n` (raw
/// mode compatible).  No ANSI cursor saves/restores are emitted — caller
/// manages cursor positioning.
fn render_picker(items: &[ModelOption], selected: usize, out: &mut impl Write) {
    // Header
    let _ = write!(
        out,
        "  \x1b[1mSelect model\x1b[0m  \
         \x1b[2m↑↓ / jk  navigate  ·  Enter  confirm  ·  Esc  cancel\x1b[0m\r\n\r\n"
    );

    // Item rows
    for (i, item) in items.iter().enumerate() {
        if i == selected {
            // Active row: cyan, bold, pointer glyph
            let _ = write!(
                out,
                "  \x1b[1;96m▶ {:<32}\x1b[0m\x1b[2m{}\x1b[0m\r\n",
                item.display, item.description,
            );
        } else {
            // Inactive row: dimmed
            let _ = write!(
                out,
                "    \x1b[2m{:<32}{}\x1b[0m\r\n",
                item.display, item.description,
            );
        }
    }

    // Trailing blank line
    let _ = write!(out, "\r\n");
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the interactive picker.  Returns the selected model **ID** (the value
/// to use in API requests), or `None` if the user cancelled (Esc / q).
///
/// `current` is compared against `ModelOption::id` to pre-highlight the
/// active model.
///
/// **Must** be called synchronously (e.g. via `tokio::task::block_in_place`).
pub fn run_picker(
    items: &[ModelOption],
    current: &str,
    stdout: &Arc<Mutex<std::io::Stdout>>,
    rows: u16,
) -> Option<String> {
    if items.is_empty() {
        return None;
    }

    let mut selected = items.iter().position(|i| i.id == current).unwrap_or(0);

    let nlines = total_lines(items.len());

    // -----------------------------------------------------------------------
    // Initial render — move to scroll-region bottom, let DECSTBM scroll the
    // region upward as each `\r\n` lands at the bottom boundary.
    // -----------------------------------------------------------------------
    {
        let mut out = stdout.lock().unwrap();
        execute!(out, cursor::MoveTo(0, rows.saturating_sub(4))).ok();
        render_picker(items, selected, &mut *out);
    }

    // -----------------------------------------------------------------------
    // Key-event loop
    // -----------------------------------------------------------------------
    loop {
        let ev = match event::read() {
            Ok(e) => e,
            Err(_) => return None,
        };

        let key = match ev {
            Event::Key(k) => k,
            _ => continue,
        };

        // Ignore key-release events (some terminals emit them in raw mode)
        if key.kind == KeyEventKind::Release {
            continue;
        }

        let prev = selected;

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if selected > 0 {
                    selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if selected < items.len() - 1 {
                    selected += 1;
                }
            }
            KeyCode::Enter => {
                let model = items[selected].id.clone();
                {
                    let mut out = stdout.lock().unwrap();
                    let _ = write!(
                        out,
                        "\x1b[2m[Switched to \x1b[0m\x1b[96m{model}\x1b[0m\x1b[2m]\x1b[0m\r\n"
                    );
                    execute!(out, cursor::MoveTo(0, rows.saturating_sub(2))).ok();
                    let _ = out.flush();
                }
                return Some(model);
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                {
                    let mut out = stdout.lock().unwrap();
                    let _ = write!(out, "\x1b[2m[Cancelled]\x1b[0m\r\n");
                    execute!(out, cursor::MoveTo(0, rows.saturating_sub(2))).ok();
                    let _ = out.flush();
                }
                return None;
            }
            _ => continue,
        }

        // -------------------------------------------------------------------
        // Redraw only when selection changed
        // -------------------------------------------------------------------
        if selected != prev {
            let mut out = stdout.lock().unwrap();
            let _ = write!(out, "\x1b[{nlines}A"); // cursor up to header row
            let _ = write!(out, "\x1b[J"); // erase to end of scroll region
            render_picker(items, selected, &mut *out);
        }
    }
}
