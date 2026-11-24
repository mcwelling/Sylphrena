//! Typing while the agent works.
//!
//! During a turn the input row shows the thinking animation, so what the
//! user types goes on the last row of the terminal (the spare "buffer" row
//! below the input row). Enter sends it to the agent as steering, which it
//! sees after the tool calls running now; Ctrl+C cancels the turn.
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use tokio::sync::mpsc;

/// What a key press means during a turn.
#[derive(Debug, Clone, PartialEq)]
pub enum SteerKey {
    /// The line changed; redraw it.
    Edited,
    /// Send this text as steering.
    Send(String),
    Cancel,
    /// Shift+Tab: switch plan/build mode (from the next turn).
    ToggleMode,
    Ignored,
}

/// The line being typed.
#[derive(Debug, Default)]
pub struct SteerLine {
    buf: String,
}

impl SteerLine {
    /// What is typed so far.
    pub fn text(&self) -> &str {
        &self.buf
    }

    /// Handle a terminal event: a key, or pasted text (kept whole, newlines
    /// and all, so a multi-line paste is one message).
    pub fn handle_event(&mut self, ev: Event) -> SteerKey {
        match ev {
            Event::Key(k) => self.handle(k),
            Event::Paste(text) => {
                self.buf.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                SteerKey::Edited
            }
            _ => SteerKey::Ignored,
        }
    }

    pub fn handle(&mut self, key: KeyEvent) -> SteerKey {
        if key.kind != KeyEventKind::Press {
            return SteerKey::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => {
                self.buf.clear();
                SteerKey::Cancel
            }
            KeyCode::BackTab => SteerKey::ToggleMode,
            KeyCode::Char('u') if ctrl => {
                self.buf.clear();
                SteerKey::Edited
            }
            KeyCode::Char(c) if !ctrl => {
                self.buf.push(c);
                SteerKey::Edited
            }
            KeyCode::Backspace => {
                self.buf.pop();
                SteerKey::Edited
            }
            KeyCode::Esc => {
                self.buf.clear();
                SteerKey::Edited
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.buf).trim().to_string();
                if text.is_empty() {
                    SteerKey::Ignored
                } else {
                    SteerKey::Send(text)
                }
            }
            _ => SteerKey::Ignored,
        }
    }

    /// Draw the line on the last row, keeping the cursor where it was.
    /// Autowrap is off while drawing, so an over-long line is clipped at the
    /// edge instead of wrapping or scrolling the screen.
    pub fn draw(&self, stdout: &Arc<Mutex<std::io::Stdout>>, rows: u16, cols: u16) {
        // Leave room for "» " and a spare column; wide characters may still
        // take two cells, which the clipping absorbs.
        let width = cols.saturating_sub(4) as usize;
        let body = if self.buf.is_empty() {
            let hint = "type to steer · Enter sends after the current tool calls · Ctrl+C cancels";
            format!("\x1b[2m{}\x1b[0m", hint.chars().take(width).collect::<String>())
        } else {
            // Show the end of a long line, on one line.
            let flat: String = self.buf.chars().map(|c| if c.is_control() { '⏎' } else { c }).collect();
            let n = flat.chars().count();
            flat.chars().skip(n.saturating_sub(width)).collect()
        };
        if let Ok(mut out) = stdout.lock() {
            let _ = write!(out, "\x1b7\x1b[?7l\x1b[{rows};1H\x1b[2K\x1b[2m»\x1b[0m {body}\x1b[?7h\x1b8");
            let _ = out.flush();
        }
    }

    /// Blank the last row.
    pub fn clear(stdout: &Arc<Mutex<std::io::Stdout>>, rows: u16) {
        if let Ok(mut out) = stdout.lock() {
            let _ = write!(out, "\x1b7\x1b[{rows};1H\x1b[2K\x1b8");
            let _ = out.flush();
        }
    }
}

/// Reads terminal events on a blocking thread until stopped, with
/// bracketed paste on so a paste arrives as one event.
pub struct KeyReader {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    pub keys: mpsc::UnboundedReceiver<Event>,
}

impl KeyReader {
    pub fn start() -> Self {
        let _ = execute!(std::io::stdout(), EnableBracketedPaste);
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, keys) = mpsc::unbounded_channel();
        let flag = stop.clone();
        let task = tokio::task::spawn_blocking(move || {
            while !flag.load(Ordering::Relaxed) {
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(ev @ (Event::Key(_) | Event::Paste(_))) => {
                            if tx.send(ev).is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });
        KeyReader { stop, task, keys }
    }

    /// Stop reading and return every event not yet handled: those queued
    /// here and those crossterm already read from the terminal but hasn't
    /// handed out (which the next `readline` would never see).
    pub async fn stop(mut self) -> Vec<Event> {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.task.await;
        let mut left = Vec::new();
        while let Ok(ev) = self.keys.try_recv() {
            left.push(ev);
        }
        while let Ok(true) = event::poll(Duration::ZERO) {
            match event::read() {
                Ok(ev @ (Event::Key(_) | Event::Paste(_))) => left.push(ev),
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        left
    }
}

/// Text to carry into the next prompt: what was being typed, plus any keys
/// left over at the end of the turn. Control keys among the leftovers
/// (Enter, Ctrl+C) are dropped: they belonged to the turn that ended.
pub fn carry_over(line: &SteerLine, left: Vec<Event>) -> String {
    let mut text = line.text().to_string();
    for ev in left {
        match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => text.push(c),
                KeyCode::Backspace => {
                    text.pop();
                }
                _ => {}
            },
            Event::Paste(p) => text.push_str(&p),
            _ => {}
        }
    }
    // rustyline's line editor is single-line.
    text.replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn typing_editing_and_sending() {
        let mut l = SteerLine::default();
        for c in "use rg".chars() {
            assert_eq!(l.handle(key(KeyCode::Char(c))), SteerKey::Edited);
        }
        l.handle(key(KeyCode::Backspace));
        assert_eq!(l.handle(key(KeyCode::Enter)), SteerKey::Send("use r".into()));
        assert_eq!(l.handle(key(KeyCode::Enter)), SteerKey::Ignored, "empty line sends nothing");
        assert_eq!(l.handle(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)), SteerKey::Cancel);
    }

    #[test]
    fn a_paste_is_one_message_and_leftovers_carry_over() {
        let mut l = SteerLine::default();
        l.handle_event(Event::Paste("line one\r\nline two".into()));
        assert_eq!(l.handle(key(KeyCode::Enter)), SteerKey::Send("line one\nline two".into()));
        l.handle(key(KeyCode::Char('a')));
        let left = vec![
            Event::Key(key(KeyCode::Char('b'))),
            Event::Key(key(KeyCode::Enter)),
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Event::Key(key(KeyCode::Char('c'))),
        ];
        assert_eq!(carry_over(&l, left), "abc");
    }
}
