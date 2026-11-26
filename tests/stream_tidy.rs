//! The TUI keeps blank lines out of streamed replies.
use syl_rs::tui::stream::StreamTidy;

/// What the screen gets for a stream of fragments.
fn shown(fragments: &[&str]) -> Vec<String> {
    let mut t = StreamTidy::default();
    fragments.iter().filter_map(|f| t.feed(f)).collect()
}

#[test]
fn blank_lines_alone_show_nothing() {
    // A request that only leads into a tool call.
    assert!(shown(&["", "\n\n", "\n"]).is_empty());
    assert!(shown(&["  ", "\n"]).is_empty());
}

#[test]
fn leading_blank_lines_go_and_indentation_stays() {
    assert_eq!(shown(&["\n\n", "Found it."]), ["Found it."]);
    assert_eq!(shown(&["\n\nFirst\nSecond"]), ["First\nSecond"]);
    assert_eq!(shown(&["\n", "  - item"]), ["  - item"]);
}

#[test]
fn trailing_blank_lines_wait_for_more_text() {
    // Kept between paragraphs...
    assert_eq!(shown(&["One.\n\n", "Two."]), ["One.", "\n\nTwo."]);
    // ...but dropped at the end, where the stream's end adds a line break.
    assert_eq!(shown(&["Let me look.\n\n", "\n"]), ["Let me look."]);
    // A fragment of only newlines mid-reply is held, not lost.
    assert_eq!(shown(&["a", "\n", "\n", "b"]), ["a", "\n\nb"]);
}

#[test]
fn a_reset_starts_a_new_stretch() {
    let mut t = StreamTidy::default();
    assert_eq!(t.feed("Checking.\n\n").as_deref(), Some("Checking."));
    t.reset();
    assert_eq!(t.feed("\n\nDone.").as_deref(), Some("Done."));
}

#[test]
fn carriage_returns_are_dropped() {
    assert_eq!(shown(&["a\r\n\r\n", "b"]), ["a", "\n\nb"]);
}
