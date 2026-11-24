/// Terminal control: scroll region setup, status bar drawing, cursor helpers, SIGWINCH.
///
/// Layout (1-indexed rows, `rows` = total terminal height):
///   Rows 1..rows-3 — scroll region (the conversation)
///   Row rows-2     — the status bar (model, tokens, cost, meters)
///   Row rows-1     — input prompt; during a turn, the thinking animation
///                    or the tool ticker (they never run at the same time)
///   Row rows       — spare (the steering line during a turn)
///
/// The scroll region starts at row 1 on purpose: terminals keep lines that
/// scroll off the top of the screen in their scrollback, but drop lines that
/// scroll off a region starting lower down. So everything fixed lives at
/// the bottom, and the conversation can be scrolled back through.
///
/// In crossterm MoveTo (0-indexed): `LAST_CHAT_ROW` etc. below.
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use crossterm::{
    cursor,
    execute, queue,
    style::Print,
    terminal::{Clear, ClearType},
};

/// The last row of the scroll region (0-indexed).
pub fn last_chat_row(rows: u16) -> u16 {
    rows.saturating_sub(4)
}

/// The status bar row (0-indexed).
pub fn bar_row(rows: u16) -> u16 {
    rows.saturating_sub(3)
}

/// The row the tool ticker and spinner use during a turn: the input row,
/// 1-indexed for escape sequences.
pub fn ticker_row_1(rows: u16) -> u16 {
    rows.saturating_sub(1)
}

/// Establish the scroll region for Syl's fixed-row layout.
///
/// Sets the DECSTBM scroll region to rows 1..=rows-3 (1-indexed) and moves
/// the cursor to the input row so the first readline call lands correctly.
pub fn setup_scroll_region(stdout: &mut impl Write, rows: u16) -> io::Result<()> {
    // DECSTBM: set scroll region top=1, bottom=rows-3 (1-indexed)
    let bot = rows.saturating_sub(3).max(1);
    write!(stdout, "\x1b[1;{}r", bot)?;
    // Move cursor to input row: rows-1 (1-indexed) = rows-2 (0-indexed)
    execute!(stdout, cursor::MoveTo(0, rows.saturating_sub(2)))?;
    stdout.flush()
}

/// Draw the status bar without moving the user-visible cursor.
///
/// Uses DECSC (\x1b7) / DECRC (\x1b8) to save and restore the cursor position
/// so bar repaints are invisible to output flowing through the scroll region.
pub fn draw_status_bar(stdout: &mut impl Write, bar: &str, rows: u16) -> io::Result<()> {
    write!(stdout, "\x1b7")?;
    queue!(stdout, cursor::MoveTo(0, bar_row(rows)), Clear(ClearType::CurrentLine), Print(bar))?;
    write!(stdout, "\x1b8")?;
    stdout.flush()
}

/// Move the cursor to the last row of the scroll region and emit `\r\n` to
/// scroll content upward before appending new output.
///
/// A `\r\n` on the region's last row scrolls the region and leaves the
/// cursor at the same logical position.
pub fn move_to_scroll(stdout: &mut impl Write, rows: u16) -> io::Result<()> {
    execute!(stdout, cursor::MoveTo(0, last_chat_row(rows)))?;
    write!(stdout, "\r\n")?;
    stdout.flush()
}

/// Move the cursor to the input row (rows-1 in 1-indexed = rows-2 in 0-indexed).
pub fn move_to_input(stdout: &mut impl Write, rows: u16) -> io::Result<()> {
    execute!(stdout, cursor::MoveTo(0, rows.saturating_sub(2)))?;
    stdout.flush()
}

/// Print `text` into the scroll region without disturbing the cursor position
/// visible to readline or the animation row.
///
/// Uses DECSC (\\x1b7) / DECRC (\\x1b8) to save and restore the cursor.
/// Moves to the last scroll-region row, emits \\r\\n to scroll content up,
/// then writes `text` (with \\n replaced by \\r\\n for raw mode).
pub fn print_to_scroll(
    stdout: &Arc<Mutex<std::io::Stdout>>,
    text: &str,
    rows: u16,
) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdout lock poisoned"))?;
    // Save cursor (DECSC)
    write!(out, "\x1b7")?;
    execute!(out, cursor::MoveTo(0, last_chat_row(rows)))?;
    // Scroll region up one line, then print text
    let normalized = text.replace('\n', "\r\n");
    write!(out, "\r\n{}", normalized)?;
    // Restore cursor (DECRC)
    write!(out, "\x1b8")?;
    out.flush()
}

// ---------------------------------------------------------------------------
// Streaming output helpers (M3 — smooth inline streaming)
// ---------------------------------------------------------------------------

/// Open a new output line at the bottom of the scroll region.
/// Called once at the start of a streaming turn (first text delta).
/// Does NOT save/restore cursor — caller must call `end_streaming` when done.
pub fn begin_streaming(
    stdout: &Arc<Mutex<std::io::Stdout>>,
    rows: u16,
) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdout lock poisoned"))?;
    // Move to last row of scroll region, scroll up to create a new blank line
    execute!(out, cursor::MoveTo(0, last_chat_row(rows)))?;
    write!(out, "\r\n")?;
    out.flush()
}

/// Append a text fragment at the current cursor position (inline, no scroll).
/// Must be called after `begin_streaming`. Converts \n to \r\n for raw mode.
pub fn write_streaming(
    stdout: &Arc<Mutex<std::io::Stdout>>,
    text: &str,
) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdout lock poisoned"))?;
    write!(out, "{}", text.replace('\n', "\r\n"))?;
    out.flush()
}

/// Complete a streaming turn: emit a trailing newline and move cursor to the
/// input row so readline redraws the prompt in the right place.
pub fn end_streaming(
    stdout: &Arc<Mutex<std::io::Stdout>>,
    rows: u16,
) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdout lock poisoned"))?;
    write!(out, "\r\n")?;
    execute!(out, cursor::MoveTo(0, rows.saturating_sub(2)))?;
    out.flush()
}

/// Show `text` on the ticker row, replacing what was there. The spinner,
/// the thinking animation or the next prompt overwrites it.
pub fn draw_ticker(stdout: &Arc<Mutex<std::io::Stdout>>, rows: u16, text: &str) {
    if let Ok(mut out) = stdout.lock() {
        // Autowrap off, so a long line is clipped rather than wrapped.
        let _ = write!(out, "\x1b7\x1b[?7l\x1b[{};1H\x1b[2K{text}\x1b[?7h\x1b8", ticker_row_1(rows));
        let _ = out.flush();
    }
}

/// Write raw bytes to stdout.
pub fn write_raw(stdout: &mut impl Write, data: &[u8]) -> io::Result<()> {
    stdout.write_all(data)?;
    stdout.flush()
}

/// Spawn a SIGWINCH listener task.
///
/// Returns a `watch::Receiver` that yields the new `(cols, rows)` every time
/// the terminal is resized.  The initial value is the current terminal size.
/// No-op on non-Unix platforms (returns a receiver that never changes).
pub fn spawn_sigwinch_handler() -> tokio::sync::watch::Receiver<(u16, u16)> {
    let initial = crossterm::terminal::size().unwrap_or((80, 24));
    let (tx, rx) = tokio::sync::watch::channel(initial);

    #[cfg(unix)]
    tokio::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = match signal(SignalKind::window_change()) {
            Ok(s) => s,
            Err(_) => return,
        };
        loop {
            sig.recv().await;
            let size = crossterm::terminal::size().unwrap_or((80, 24));
            if tx.send(size).is_err() {
                break;
            }
        }
    });

    rx
}
