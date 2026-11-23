use crate::api::types::Message;
use std::fs;
use std::path::Path;

pub fn save_session(session_path: &Path, messages: &[Message]) -> Result<(), String> {
    if let Some(parent) = session_path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(messages).map_err(|e| e.to_string())?;
    fs::write(session_path, json).map_err(|e| e.to_string())
}

pub fn load_session(session_path: &Path) -> Option<Vec<Message>> {
    let content = fs::read_to_string(session_path).ok()?;
    serde_json::from_str(&content).ok()
}
