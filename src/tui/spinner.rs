/// Tool-execution spinner: Braille frames on the bottom bar row.
///
/// Runs at 100 ms/frame and displays elapsed time.  Uses the same ESC7/ESC8
/// save-restore pattern as the status bars so it doesn't disturb cursor.
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const BRAILLE: &[&str] = &[
    "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏",
];

/// Handle for a running spinner task.
pub struct SpinnerHandle {
    cancel: CancellationToken,
}

impl SpinnerHandle {
    /// Stop the spinner and clear the bottom bar row.
    pub fn stop(self) {
        self.cancel.cancel();
    }
}

/// Spawn the tool-execution spinner on the bottom bar row.
///
/// `rows` is the current terminal height (crossterm `size().1`).
/// `label` is the tool name or short description shown next to the spinner.
///
/// The task overwrites the bottom bar row (rows-2 in 1-indexed = rows-3 in
/// 0-indexed) every 100 ms.  Stop it with [`SpinnerHandle::stop`].
pub fn spawn_spinner(
    stdout: Arc<Mutex<std::io::Stdout>>,
    rows: u16,
    label: String,
) -> SpinnerHandle {
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();

    // Bottom bar: rows-2 (1-indexed) = rows-3 (0-indexed)
    let bottom_row_1indexed = rows.saturating_sub(2);

    tokio::spawn(async move {
        let start = Instant::now();
        let mut frame = 0usize;

        loop {
            tokio::select! {
                _ = cancel2.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
            }

            let elapsed = start.elapsed().as_secs();
            let spin = BRAILLE[frame % BRAILLE.len()];
            let line = format!(" {} {} ({} s)", spin, label, elapsed);

            if let Ok(mut out) = stdout.lock() {
                let _ = write!(
                    out,
                    "\x1b7\x1b[{};1H\x1b[2K{}\x1b8",
                    bottom_row_1indexed, line,
                );
                let _ = out.flush();
            }

            frame = frame.wrapping_add(1);
        }

        // Clear spinner row on exit
        if let Ok(mut out) = stdout.lock() {
            let _ = write!(out, "\x1b7\x1b[{};1H\x1b[2K\x1b8", bottom_row_1indexed);
            let _ = out.flush();
        }
    });

    SpinnerHandle { cancel }
}
