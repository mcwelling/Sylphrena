/// Readline input wrapper using rustyline.
///
/// The key invariant: the cursor is explicitly moved to the input row (rows-1
/// in 1-indexed = rows-2 in 0-indexed) before every `readline` call.  After
/// tool output scrolls through the scroll region, the cursor can end up inside
/// it; without this anchor, rustyline draws the prompt at the wrong row.
use std::io;

use std::sync::{Arc, Mutex};

use crossterm::{cursor, execute};
use rustyline::{
    Cmd, ConditionalEventHandler, Config, DefaultEditor, Event, EventContext, EventHandler, KeyCode, KeyEvent,
    Modifiers, RepeatCount, Result as RlResult,
};

/// A key at the prompt that does something rather than edit the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// Shift+Tab: switch between plan and build mode.
    ToggleMode,
    /// Ctrl+Z: undo the last turn's file changes.
    Undo,
}

/// Records the hotkey and accepts the line, so `readline` returns and the
/// caller can act on it (the typed text comes back with it, to restore).
struct HotkeyHandler {
    key: Hotkey,
    pressed: Arc<Mutex<Option<Hotkey>>>,
}

impl ConditionalEventHandler for HotkeyHandler {
    fn handle(&self, _: &Event, _: RepeatCount, _: bool, _: &EventContext) -> Option<Cmd> {
        *self.pressed.lock().unwrap() = Some(self.key);
        Some(Cmd::AcceptLine)
    }
}

pub struct InputEditor {
    rl: DefaultEditor,
    pressed: Arc<Mutex<Option<Hotkey>>>,
}

impl InputEditor {
    /// Create a new editor.  History persistence is disabled (matching syl.bb).
    pub fn new() -> RlResult<Self> {
        let config = Config::builder()
            .history_ignore_space(true)
            .max_history_size(0)? // disable history persistence
            .build();
        let mut rl = DefaultEditor::with_config(config)?;
        let pressed = Arc::new(Mutex::new(None));
        for (key, hotkey) in [
            (KeyEvent(KeyCode::BackTab, Modifiers::NONE), Hotkey::ToggleMode),
            (KeyEvent::ctrl('z'), Hotkey::Undo),
        ] {
            rl.bind_sequence(
                key,
                EventHandler::Conditional(Box::new(HotkeyHandler { key: hotkey, pressed: pressed.clone() })),
            );
        }
        Ok(Self { rl, pressed })
    }

    /// The hotkey that ended the last `readline`, if one did.
    pub fn take_hotkey(&self) -> Option<Hotkey> {
        self.pressed.lock().unwrap().take()
    }

    /// Read a line from the user.
    ///
    /// Moves the cursor to the input row before calling rustyline so the prompt
    /// always appears on the correct row regardless of prior scroll activity.
    /// `initial` is put in the line to start with (text typed at the end
    /// of a turn, say).
    pub fn readline(&mut self, prompt: &str, rows: u16, initial: &str) -> RlResult<String> {
        // Anchor cursor at input row: rows-1 (1-indexed) = rows-2 (0-indexed)
        // Clear it too: the tool ticker may have left its last line here.
        let _ = execute!(
            io::stdout(),
            cursor::MoveTo(0, rows.saturating_sub(2)),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::CurrentLine)
        );
        if initial.is_empty() {
            self.rl.readline(prompt)
        } else {
            self.rl.readline_with_initial(prompt, (initial, ""))
        }
    }

    /// Add an entry to the in-memory history (not persisted).
    pub fn add_history_entry(&mut self, line: &str) -> bool {
        self.rl.add_history_entry(line).unwrap_or(false)
    }
}
