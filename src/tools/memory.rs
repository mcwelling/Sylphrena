use std::fs;
use std::path::Path;

/// Write a memory entry. Creates the directory if needed.
pub fn memory_write(memory_dir: &Path, key: &str, content: &str) -> String {
    if let Err(e) = fs::create_dir_all(memory_dir) {
        return format!("Error creating memory dir: {}", e);
    }
    let path = memory_dir.join(format!("{}.md", key));
    match fs::write(&path, content) {
        Ok(())  => format!("Remembered '{}'. Written to {}", key, path.display()),
        Err(e)  => format!("Error writing memory: {}", e),
    }
}

/// Read memory entries.
/// Blank key → all memories concatenated.
/// Specific key → contents of that file.
pub fn memory_read(memory_dir: &Path, key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        // List all
        match fs::read_dir(memory_dir) {
            Err(_) => return "No memories stored yet.".to_string(),
            Ok(entries) => {
                let mut files: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".md"))
                    .collect();
                if files.is_empty() {
                    return "No memories stored yet.".to_string();
                }
                files.sort();
                let parts: Vec<String> = files.iter().map(|fname| {
                    let k = fname.trim_end_matches(".md");
                    let content = fs::read_to_string(memory_dir.join(fname)).unwrap_or_default();
                    format!("## {}\n{}", k, content)
                }).collect();
                parts.join("\n\n")
            }
        }
    } else {
        let path = memory_dir.join(format!("{}.md", key));
        match fs::read_to_string(&path) {
            Ok(s)  => s,
            Err(_) => format!("No memory found for '{}'.", key),
        }
    }
}

/// List memory keys (file stems of all .md files in `memory_dir`), sorted.
pub fn list_memories(memory_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(memory_dir) else { return vec![] };
    let mut keys: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".md"))
        .map(|n| n.trim_end_matches(".md").to_string())
        .collect();
    keys.sort();
    keys
}

/// Load all memories as a single string, or None if the directory is empty/absent.
pub fn load_all_memories(memory_dir: &Path) -> Option<String> {
    let entries = fs::read_dir(memory_dir).ok()?;
    let mut files: Vec<String> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".md"))
        .collect();
    if files.is_empty() {
        return None;
    }
    files.sort();
    let parts: Vec<String> = files.iter().map(|fname| {
        let k = fname.trim_end_matches(".md");
        let content = fs::read_to_string(memory_dir.join(fname)).unwrap_or_default();
        format!("## {}\n{}", k, content)
    }).collect();
    Some(parts.join("\n\n"))
}
