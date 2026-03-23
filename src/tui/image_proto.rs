/// Terminal image protocol detection and tmux passthrough wrapping.
///
/// Mirrors syl.bb's `detect-image-protocol` function.  Used by `tools/image.rs`
/// to choose the right rendering path (Kitty, iTerm2, Sixel, or symbols).
use std::env;

/// Supported image rendering protocols.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    /// Kitty APC pixel protocol (`\x1b_G…\x1b\\`).
    Kitty,
    /// iTerm2 OSC inline protocol (`\x1b]1337;File=…\x07`).
    Iterm2,
    /// DEC Sixel graphics.
    Sixel,
    /// Character-art fallback (chafa `--format=symbols`).
    Symbols,
}

impl std::fmt::Display for ImageProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageProtocol::Kitty   => write!(f, "kitty"),
            ImageProtocol::Iterm2  => write!(f, "iterm2"),
            ImageProtocol::Sixel   => write!(f, "sixel"),
            ImageProtocol::Symbols => write!(f, "symbols"),
        }
    }
}

/// Detect the image protocol from environment variables.
///
/// Priority:
///   1. `SYL_IMAGE` env var — explicit user override
///   2. `TERM` / `TERM_PROGRAM` — terminal identification heuristics
///   3. Fallback: `Symbols`
pub fn detect_image_protocol() -> ImageProtocol {
    // Explicit override
    if let Ok(val) = env::var("SYL_IMAGE") {
        return parse_protocol_env(&val);
    }

    let term = env::var("TERM").unwrap_or_default();
    let term_program = env::var("TERM_PROGRAM").unwrap_or_default();
    let colorterm = env::var("COLORTERM").unwrap_or_default();

    if term.contains("kitty") || term == "xterm-kitty" {
        return ImageProtocol::Kitty;
    }

    if term_program == "ghostty" {
        return ImageProtocol::Kitty;
    }

    if term_program == "iTerm.app" {
        return ImageProtocol::Iterm2;
    }

    if term.contains("sixel") || colorterm == "sixel" {
        return ImageProtocol::Sixel;
    }

    ImageProtocol::Symbols
}

/// Parse a protocol name string (case-insensitive).
///
/// Accepts `"kitty"`, `"iterm2"`, `"sixel"`, `"symbols"` (and `"auto"` which
/// re-runs detection).  Returns `Symbols` for unrecognised values.
pub fn parse_protocol_env(s: &str) -> ImageProtocol {
    match s.trim().to_lowercase().as_str() {
        "kitty"   => ImageProtocol::Kitty,
        "iterm2"  => ImageProtocol::Iterm2,
        "sixel"   => ImageProtocol::Sixel,
        "symbols" | "symbol" | "text" => ImageProtocol::Symbols,
        "auto"    => detect_image_protocol(),
        _         => ImageProtocol::Symbols,
    }
}

/// Return true if the session is running inside tmux.
pub fn is_tmux() -> bool {
    env::var("TMUX").map(|v| !v.is_empty()).unwrap_or(false)
}

/// Wrap terminal escape sequences for tmux passthrough (DCS wrapping).
///
/// tmux requires that any escape sequences meant for the outer terminal are
/// wrapped in a Device Control String (DCS) passthrough.  Every `\x1b` inside
/// the payload must be doubled.
///
/// ```text
/// Outer terminal sees: ESC P tmux; <escaped-payload> ESC \\
/// ```
///
/// Use this when sending Kitty APC or iTerm2 OSC sequences through tmux.
pub fn wrap_for_tmux(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2 + 16);
    // Start tmux DCS passthrough
    out.extend_from_slice(b"\x1bPtmux;");
    // Escape every ESC byte in the payload
    for &b in data {
        if b == 0x1b {
            out.push(0x1b); // doubled ESC
        }
        out.push(b);
    }
    // End DCS
    out.extend_from_slice(b"\x1b\\");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kitty() {
        assert_eq!(parse_protocol_env("kitty"), ImageProtocol::Kitty);
        assert_eq!(parse_protocol_env("KITTY"), ImageProtocol::Kitty);
    }

    #[test]
    fn parse_iterm2() {
        assert_eq!(parse_protocol_env("iterm2"), ImageProtocol::Iterm2);
    }

    #[test]
    fn parse_sixel() {
        assert_eq!(parse_protocol_env("sixel"), ImageProtocol::Sixel);
    }

    #[test]
    fn parse_symbols() {
        assert_eq!(parse_protocol_env("symbols"), ImageProtocol::Symbols);
        assert_eq!(parse_protocol_env("text"), ImageProtocol::Symbols);
        assert_eq!(parse_protocol_env("unknown"), ImageProtocol::Symbols);
    }

    #[test]
    fn display_names() {
        assert_eq!(ImageProtocol::Kitty.to_string(),   "kitty");
        assert_eq!(ImageProtocol::Iterm2.to_string(),  "iterm2");
        assert_eq!(ImageProtocol::Sixel.to_string(),   "sixel");
        assert_eq!(ImageProtocol::Symbols.to_string(), "symbols");
    }

    #[test]
    fn tmux_wrap_doubles_esc() {
        // Input: ESC _ G data ESC \\  (a minimal Kitty APC)
        let input = b"\x1b_Gfoo\x1b\\";
        let wrapped = wrap_for_tmux(input);

        // Must start with DCS passthrough prefix
        assert!(wrapped.starts_with(b"\x1bPtmux;"));
        // Must end with DCS terminator
        assert!(wrapped.ends_with(b"\x1b\\"));

        // Count ESC bytes in the payload section (between prefix and suffix)
        let prefix_len = b"\x1bPtmux;".len();
        let suffix_len = b"\x1b\\".len();
        let payload = &wrapped[prefix_len..wrapped.len() - suffix_len];

        // Original had 2 ESC bytes; wrapped payload should have 4 (each doubled)
        let esc_count = payload.iter().filter(|&&b| b == 0x1b).count();
        assert_eq!(esc_count, 4, "each ESC should be doubled");
    }

    #[test]
    fn tmux_wrap_passthrough_roundtrip() {
        // Any non-ESC byte should appear unchanged in the wrapped payload
        let input = b"hello world";
        let wrapped = wrap_for_tmux(input);
        let prefix = b"\x1bPtmux;";
        let suffix = b"\x1b\\";
        let payload = &wrapped[prefix.len()..wrapped.len() - suffix.len()];
        assert_eq!(payload, b"hello world");
    }
}
