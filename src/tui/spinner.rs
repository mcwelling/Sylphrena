/// Tool-execution spinner: Braille frames on the ticker row (the input row
/// during a turn).
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
    /// Stop the spinner. The row keeps its last frame until redrawn.
    pub fn stop(self) {
        self.cancel.cancel();
    }
}

/// Spawn the tool-execution spinner on the ticker row.
///
/// `rows` is the current terminal height (crossterm `size().1`).
/// `label` is the tool name or short description shown next to the spinner.
///
/// The task overwrites the ticker row every 100 ms.  Stop it with
/// [`SpinnerHandle::stop`].
pub fn spawn_spinner(
    stdout: Arc<Mutex<std::io::Stdout>>,
    rows: u16,
    label: String,
) -> SpinnerHandle {
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();

    let row_1indexed = crate::tui::terminal::ticker_row_1(rows);

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
                // Stopped while waiting for the lock? Then whoever stopped it
                // is about to draw (or has drawn) this row: don't paint over it.
                // `stop` signals before the caller takes the lock to draw, so
                // checking under the lock closes the gap.
                if cancel2.is_cancelled() {
                    break;
                }
                // Autowrap off: a long label is clipped, never wrapped.
                let _ = write!(
                    out,
                    "\x1b7\x1b[?7l\x1b[{};1H\x1b[2K{}\x1b[?7h\x1b8",
                    row_1indexed, line,
                );
                let _ = out.flush();
            }

            frame = frame.wrapping_add(1);
        }
        // The row is left as it is: whoever stopped the spinner draws it
        // next (the tool ticker, or the bars), and a clear from this task
        // could land after that and wipe it.
    });

    SpinnerHandle { cancel }
}
