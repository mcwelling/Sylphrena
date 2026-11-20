/// `read_file` tool — reads a paged range of lines from a file inside the container.
/// Mirrors `read-file-paged` in syl.bb.
use crate::config::{container_name, AppConfig};
use crate::util::{shell_quote, truncate_output};

/// Run the `read_file` tool. Returns `(output, is_error)`.
///
/// Constructs a shell script that:
/// 1. Normalises the path (relative paths are resolved under `/workspace/`)
/// 2. Checks the file exists
/// 3. Prints a header with total line count and the requested range
/// 4. Uses `awk` to print lines in range with 5-char right-aligned line numbers
///
/// Default window: 100 lines (start_line..start_line+99).
pub fn read_file(input: &serde_json::Value, config: &AppConfig) -> (String, bool) {
    let path = match input["path"].as_str() {
        Some(p) if !p.is_empty() => p,
        _ => return ("[Tool error: read_file requires a non-empty 'path' field]".to_string(), true),
    };

    let start: u64 = input["start_line"].as_u64().unwrap_or(1).max(1);
    let end: u64   = input["end_line"].as_u64().unwrap_or(start + 99);

    // Normalise: relative paths live under /workspace/ inside the container
    let abs_path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/workspace/{}", path)
    };

    // Mirror the syl.bb command exactly
    let cmd = format!(
        "f={abs}; \
         if [ ! -f \"$f\" ]; then echo \"Error: file not found: $f\"; exit 1; fi; \
         total=$(wc -l < \"$f\"); \
         echo \"[File: $f — $total lines total, showing {start}-{end}]\"; \
         awk 'NR>={start}{{printf \"%5d: %s\\n\", NR, $0}} NR>{end}{{exit}}' \"$f\"",
        abs   = shell_quote(&abs_path),
        start = start,
        end   = end,
    );

    let _ = config.timeout_ms; // timeout enforced at container level via bash timeout if needed
    match std::process::Command::new("podman")
        .args(["exec", container_name(), "/bin/bash", "-c", &cmd])
        .output()
    {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if out.status.success() {
                (truncate_output(&stdout), false)
            } else {
                let code = out.status.code().unwrap_or(-1);
                let parts: Vec<&str> = [stdout.trim(), stderr.trim()]
                    .iter()
                    .filter(|s| !s.is_empty())
                    .copied()
                    .collect();
                (format!("Error (Exit {}): {}", code, parts.join("\n")), true)
            }
        }
        Err(e) => (format!("[Tool error: failed to spawn podman exec: {}]", e), true),
    }
}
