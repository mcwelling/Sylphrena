//! Keeping blank lines out of the conversation while a reply streams.
//!
//! A model request that leads into a tool call often streams nothing but
//! blank lines or empty text, and replies often start or end with them.
//! Shown as they come, every such request opened a new line in the
//! scrollback and left a tall gap between the prompt and the answer. So
//! text is held back until something visible arrives, leading blank lines
//! are dropped, and trailing ones wait until more text follows (the end of
//! the stream adds its own line break).

/// What to write for each streamed fragment of one stretch of output.
#[derive(Debug, Default)]
pub struct StreamTidy {
    /// Something visible has been shown in this stretch.
    started: bool,
    /// Whitespace that came before anything visible.
    held: String,
    /// Newlines at the end of what was shown, not written yet.
    pending_newlines: usize,
}

impl StreamTidy {
    /// What to write for `text`, or None when there is nothing to show yet.
    pub fn feed(&mut self, text: &str) -> Option<String> {
        let text = text.replace('\r', "");
        let mut out = if self.started {
            "\n".repeat(std::mem::take(&mut self.pending_newlines)) + &text
        } else {
            self.held.push_str(&text);
            if self.held.trim().is_empty() {
                return None;
            }
            self.started = true;
            // Leading blank lines go; indentation on the first line stays.
            let held = std::mem::take(&mut self.held);
            let visible = held.find(|c: char| !c.is_whitespace()).unwrap_or(0);
            let first_line = held[..visible].rfind('\n').map_or(0, |i| i + 1);
            held[first_line..].to_string()
        };
        let body = out.trim_end_matches('\n').len();
        self.pending_newlines = out.len() - body;
        out.truncate(body);
        (!out.is_empty()).then_some(out)
    }

    /// The stretch ended (a tool call, a notice, the end of the turn).
    pub fn reset(&mut self) {
        *self = StreamTidy::default();
    }
}
