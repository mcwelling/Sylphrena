/// Phase 3 — TUI module tests.
///
/// Only pure functions (no I/O, no tokio runtime) are tested here.
/// Spawning functions (spawn_animation, spawn_spinner, etc.) require a live
/// terminal and are tested manually or in integration tests.
use syl_rs::tui::{
    animation::make_wind_frames,
    bars::{ansi_visible_len, make_bottom_bar, make_top_bar, pad_to, BarData, DARK, LIGHT},
    image_proto::{parse_protocol_env, wrap_for_tmux, ImageProtocol},
};

// ── animation ────────────────────────────────────────────────────────────────

#[test]
fn wind_frames_not_empty() {
    assert!(!make_wind_frames(40).is_empty());
}

#[test]
fn wind_frames_correct_count() {
    // forward (w) + reverse (w) = 2 * w  (after clamping)
    for &w in &[10u16, 20, 50] {
        let frames = make_wind_frames(w);
        assert_eq!(
            frames.len(),
            2 * w as usize,
            "expected {} frames for width {}",
            2 * w,
            w
        );
    }
}

#[test]
fn wind_frames_char_width() {
    // Every frame must be exactly the clamped width in chars.
    for &w in &[10u16, 30, 50] {
        let frames = make_wind_frames(w);
        for (i, frame) in frames.iter().enumerate() {
            assert_eq!(
                frame.chars().count(),
                w as usize,
                "frame {} has wrong width for w={}",
                i,
                w
            );
        }
    }
}

#[test]
fn wind_frames_clamped_min() {
    // Width < 10 is clamped to 10
    let frames = make_wind_frames(1);
    assert_eq!(frames[0].chars().count(), 10);
    assert_eq!(frames.len(), 20);
}

#[test]
fn wind_frames_clamped_max() {
    // Width > 50 is clamped to 50
    let frames = make_wind_frames(999);
    assert_eq!(frames[0].chars().count(), 50);
    assert_eq!(frames.len(), 100);
}

#[test]
fn wind_frames_contain_wave_char() {
    for frame in make_wind_frames(20) {
        assert!(frame.contains('≋'), "missing wave char: {:?}", frame);
    }
}

#[test]
fn wind_frames_first_wave_at_start() {
    // First frame: wave at position 0
    let frames = make_wind_frames(10);
    let first: Vec<char> = frames[0].chars().collect();
    assert_eq!(first[0], '≋');
}

#[test]
fn wind_frames_last_forward_wave_at_end() {
    // Frame w-1 (last forward): wave at position w-1
    let w = 10usize;
    let frames = make_wind_frames(w as u16);
    let frame: Vec<char> = frames[w - 1].chars().collect();
    assert_eq!(frame[w - 1], '≋');
}

// ── bars ─────────────────────────────────────────────────────────────────────

fn default_bar_data() -> BarData {
    BarData {
        model: "claude-opus-4-6".into(),
        timeout_ms: 60_000,
        input_tokens: 12_345,
        output_tokens: 1_234,
        cache_read_tokens: 8_000,
        cache_write_tokens: 500,
        tokens_per_min: 30_000.0,
        tpm_limit: 60_000.0,
        cost_usd: 0.1234,
        cpu_pct: 12.5,
        mem_mb: 256,
        message_count: 7,
        width: 100,
        spinner_label: None,
    }
}

#[test]
fn top_bar_not_empty() {
    let bar = make_top_bar(&default_bar_data(), &DARK);
    assert!(!bar.is_empty());
}

#[test]
fn top_bar_contains_token_counts() {
    let bar = make_top_bar(&default_bar_data(), &DARK);
    // 12_345 input → "12K"
    assert!(bar.contains("12K"), "top bar missing input tokens: {}", bar);
    // 1_234 output → "1K"
    assert!(bar.contains("1K"), "top bar missing output tokens: {}", bar);
}

#[test]
fn top_bar_contains_cost() {
    let bar = make_top_bar(&default_bar_data(), &DARK);
    assert!(bar.contains("0.1234"), "top bar missing cost: {}", bar);
}

#[test]
fn top_bar_padded_to_width() {
    let data = BarData { width: 120, ..default_bar_data() };
    let bar = make_top_bar(&data, &DARK);
    let visible = ansi_visible_len(&bar);
    assert!(
        visible >= 120,
        "top bar visible len {} < width 120",
        visible
    );
}

#[test]
fn top_bar_tpm_zero_limit() {
    // With tpm_limit=0, bar should not panic and should still render
    let data = BarData { tpm_limit: 0.0, ..default_bar_data() };
    let bar = make_top_bar(&data, &DARK);
    assert!(!bar.is_empty());
}

#[test]
fn bottom_bar_shows_model() {
    let bar = make_bottom_bar(&default_bar_data(), &DARK);
    assert!(bar.contains("claude-opus-4-6"), "bottom bar missing model: {}", bar);
}

#[test]
fn bottom_bar_shows_timeout() {
    let bar = make_bottom_bar(&default_bar_data(), &DARK);
    assert!(bar.contains("60"), "bottom bar missing timeout: {}", bar);
}

#[test]
fn bottom_bar_spinner_overrides_model() {
    let data = BarData {
        spinner_label: Some("⠋ run_bash (3 s)".into()),
        ..default_bar_data()
    };
    let bar = make_bottom_bar(&data, &DARK);
    assert!(
        bar.contains("run_bash"),
        "spinner label missing: {}",
        bar
    );
    // Model name should NOT appear when spinner is active
    assert!(
        !bar.contains("claude-opus-4-6"),
        "model should not appear with spinner: {}",
        bar
    );
}

#[test]
fn light_palette_renders() {
    let bar = make_top_bar(&default_bar_data(), &LIGHT);
    assert!(!bar.is_empty());
}

#[test]
fn pad_to_shorter_string() {
    let padded = pad_to("hi", 10);
    assert_eq!(ansi_visible_len(&padded), 10);
}

#[test]
fn pad_to_exact_width() {
    let padded = pad_to("hello", 5);
    assert_eq!(padded, "hello");
}

#[test]
fn pad_to_already_longer() {
    // Strings longer than width are returned unchanged
    let padded = pad_to("hello world", 5);
    assert_eq!(padded, "hello world");
}

#[test]
fn ansi_len_strips_codes() {
    let s = "\x1b[96mhello\x1b[0m";
    assert_eq!(ansi_visible_len(s), 5);
}

// ── image_proto ───────────────────────────────────────────────────────────────

#[test]
fn parse_protocol_kitty_case_insensitive() {
    assert_eq!(parse_protocol_env("kitty"), ImageProtocol::Kitty);
    assert_eq!(parse_protocol_env("KITTY"), ImageProtocol::Kitty);
    assert_eq!(parse_protocol_env("Kitty"), ImageProtocol::Kitty);
}

#[test]
fn parse_protocol_iterm2() {
    assert_eq!(parse_protocol_env("iterm2"), ImageProtocol::Iterm2);
}

#[test]
fn parse_protocol_sixel() {
    assert_eq!(parse_protocol_env("sixel"), ImageProtocol::Sixel);
}

#[test]
fn parse_protocol_symbols_variants() {
    assert_eq!(parse_protocol_env("symbols"), ImageProtocol::Symbols);
    assert_eq!(parse_protocol_env("symbol"), ImageProtocol::Symbols);
    assert_eq!(parse_protocol_env("text"), ImageProtocol::Symbols);
}

#[test]
fn parse_protocol_unknown_falls_back() {
    assert_eq!(parse_protocol_env("unknown_protocol"), ImageProtocol::Symbols);
    assert_eq!(parse_protocol_env(""), ImageProtocol::Symbols);
}

#[test]
fn image_protocol_display() {
    assert_eq!(ImageProtocol::Kitty.to_string(), "kitty");
    assert_eq!(ImageProtocol::Iterm2.to_string(), "iterm2");
    assert_eq!(ImageProtocol::Sixel.to_string(), "sixel");
    assert_eq!(ImageProtocol::Symbols.to_string(), "symbols");
}

#[test]
fn tmux_wrap_has_dcs_prefix_and_suffix() {
    let wrapped = wrap_for_tmux(b"payload");
    assert!(wrapped.starts_with(b"\x1bPtmux;"), "missing DCS prefix");
    assert!(wrapped.ends_with(b"\x1b\\"), "missing DCS suffix");
}

#[test]
fn tmux_wrap_doubles_esc_bytes() {
    // One ESC in the input → two ESC bytes in the payload section
    let wrapped = wrap_for_tmux(b"\x1bfoo");
    let prefix = b"\x1bPtmux;";
    let suffix = b"\x1b\\";
    let payload = &wrapped[prefix.len()..wrapped.len() - suffix.len()];
    let esc_count = payload.iter().filter(|&&b| b == 0x1b).count();
    assert_eq!(esc_count, 2, "one ESC input should produce two ESC in payload");
}

#[test]
fn tmux_wrap_non_esc_unchanged() {
    let input = b"hello";
    let wrapped = wrap_for_tmux(input);
    let prefix = b"\x1bPtmux;";
    let suffix = b"\x1b\\";
    let payload = &wrapped[prefix.len()..wrapped.len() - suffix.len()];
    assert_eq!(payload, b"hello");
}

#[test]
fn tmux_wrap_empty_input() {
    let wrapped = wrap_for_tmux(b"");
    assert!(wrapped.starts_with(b"\x1bPtmux;"));
    assert!(wrapped.ends_with(b"\x1b\\"));
}

#[test]
fn tmux_wrap_kitty_apc_has_four_esc() {
    // Minimal Kitty APC: ESC _ G data ESC \\  → 2 ESC bytes
    let kitty = b"\x1b_Gfoo\x1b\\";
    let wrapped = wrap_for_tmux(kitty);
    let prefix = b"\x1bPtmux;";
    let suffix = b"\x1b\\";
    let payload = &wrapped[prefix.len()..wrapped.len() - suffix.len()];
    let esc_count = payload.iter().filter(|&&b| b == 0x1b).count();
    assert_eq!(esc_count, 4, "2 ESC in Kitty APC → 4 ESC in wrapped payload");
}
