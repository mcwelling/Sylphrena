/// Thinking animation: a wind wave ("≋") with a dot trail that bounces across
/// the input row while the agent is processing.
///
/// The animation runs on the input row (rows-1 in 1-indexed = rows-2 in
/// 0-indexed) and is stopped before each readline call.
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Generate the full set of animation frames for a given terminal width.
///
/// Width is clamped to 10–50 characters (matching syl.bb's `max 10 (min 50 w)`).
/// Each frame is a string of exactly `clamped_width` visible characters.
///
/// The wave bounces left-to-right then right-to-left, producing a seamless loop.
pub fn make_wind_frames(width: u16) -> Vec<String> {
    let w = (width.max(10).min(50)) as usize;
    let mut frames: Vec<String> = Vec::with_capacity(2 * w);

    // Forward pass: "≋" moves left → right, "·" trail behind
    for pos in 0..w {
        let mut chars: Vec<char> = vec![' '; w];
        chars[pos] = '≋';
        for trail in 1..=3usize {
            if pos >= trail {
                chars[pos - trail] = '·';
            }
        }
        frames.push(chars.into_iter().collect());
    }

    // Reverse pass: "≋" moves right → left, "·" trail behind (ahead in index)
    for pos in (0..w).rev() {
        let mut chars: Vec<char> = vec![' '; w];
        chars[pos] = '≋';
        for trail in 1..=3usize {
            if pos + trail < w {
                chars[pos + trail] = '·';
            }
        }
        frames.push(chars.into_iter().collect());
    }

    frames
}

/// Handle for a running animation task.
pub struct AnimHandle {
    cancel: CancellationToken,
}

impl AnimHandle {
    /// Stop the animation and clear the input row.
    pub fn stop(self) {
        self.cancel.cancel();
    }
}

/// Spawn the thinking animation on the input row.
///
/// `rows` is the current terminal height (crossterm `size().1`).
/// `term_width` is the terminal column count for frame sizing.
///
/// The task writes to `stdout` every 80 ms.  Stop it with [`AnimHandle::stop`]
/// before calling readline.
pub fn spawn_animation(
    stdout: Arc<Mutex<std::io::Stdout>>,
    rows: u16,
    term_width: u16,
) -> AnimHandle {
    let cancel = CancellationToken::new();
    let cancel2 = cancel.clone();

    let frames = make_wind_frames(term_width);
    // Input row: rows-1 (1-indexed) = rows-2 (0-indexed)
    let input_row = rows.saturating_sub(2);

    tokio::spawn(async move {
        let mut idx = 0usize;
        loop {
            tokio::select! {
                _ = cancel2.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(80)) => {}
            }

            if let Ok(mut out) = stdout.lock() {
                let frame = &frames[idx % frames.len()];
                // ESC7, move to input row, clear line, print frame, ESC8
                let _ = write!(
                    out,
                    "\x1b7\x1b[{};1H\x1b[2K{}\x1b8",
                    input_row + 1, // escape sequences use 1-indexed rows
                    frame,
                );
                let _ = out.flush();
            }

            idx = idx.wrapping_add(1);
        }

        // Clear the animation row on exit
        if let Ok(mut out) = stdout.lock() {
            let _ = write!(out, "\x1b7\x1b[{};1H\x1b[2K\x1b8", input_row + 1);
            let _ = out.flush();
        }
    });

    AnimHandle { cancel }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_not_empty() {
        let frames = make_wind_frames(40);
        assert!(!frames.is_empty());
    }

    #[test]
    fn frames_width_matches() {
        // Each frame should be exactly w characters (where w is clamped width)
        let w = 30u16;
        let frames = make_wind_frames(w);
        for frame in &frames {
            assert_eq!(
                frame.chars().count(),
                w as usize,
                "frame has wrong char count: {:?}",
                frame
            );
        }
    }

    #[test]
    fn frames_clamped_min() {
        // Width below 10 should be clamped to 10
        let frames = make_wind_frames(3);
        assert_eq!(frames[0].chars().count(), 10);
    }

    #[test]
    fn frames_clamped_max() {
        // Width above 50 should be clamped to 50
        let frames = make_wind_frames(200);
        assert_eq!(frames[0].chars().count(), 50);
    }

    #[test]
    fn frames_contain_wave_char() {
        let frames = make_wind_frames(20);
        for frame in &frames {
            assert!(frame.contains('≋'), "frame missing wave char: {:?}", frame);
        }
    }

    #[test]
    fn frames_loop_count() {
        // forward pass (w frames) + reverse pass (w frames) = 2*w
        let w = 20u16;
        let frames = make_wind_frames(w);
        assert_eq!(frames.len(), 2 * w as usize);
    }
}
