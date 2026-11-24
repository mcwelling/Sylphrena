//! Status bar rendering: one bar with the model, tokens, cost and meters.
//!
//! All functions are pure — no I/O.  The caller owns stdout and calls
//! `draw_status_bar` from `tui::terminal` to paint the returned string.

/// ANSI colour escape strings for one theme.
pub struct Palette {
    pub cy:  &'static str, // cyan
    pub ye:  &'static str, // yellow
    pub wh:  &'static str, // white / bright
    pub mg:  &'static str, // magenta
    pub lb:  &'static str, // light-blue bold
    pub dim: &'static str, // dim
    pub b:   &'static str, // bold
    pub r:   &'static str, // reset
    pub ur:  &'static str, // urgent / red
    pub uy:  &'static str, // urgent yellow
    pub ug:  &'static str, // urgent green
}

pub const DARK: Palette = Palette {
    cy:  "\x1b[96m",
    ye:  "\x1b[93m",
    wh:  "\x1b[97m",
    mg:  "\x1b[95m",
    lb:  "\x1b[1;94m",
    dim: "\x1b[2m",
    b:   "\x1b[1m",
    r:   "\x1b[0m",
    ur:  "\x1b[91m",
    uy:  "\x1b[93m",
    ug:  "\x1b[92m",
};

pub const LIGHT: Palette = Palette {
    cy:  "\x1b[36m",
    ye:  "\x1b[33m",
    wh:  "\x1b[30m",
    mg:  "\x1b[35m",
    lb:  "\x1b[1;34m",
    dim: "\x1b[2m",
    b:   "\x1b[1m",
    r:   "\x1b[0m",
    ur:  "\x1b[31m",
    uy:  "\x1b[33m",
    ug:  "\x1b[32m",
};

/// All data needed to render the bar.
pub struct BarData {
    /// Model identifier.
    pub model: String,
    /// Cumulative input tokens for this session.
    pub input_tokens: u64,
    /// Cumulative output tokens for this session.
    pub output_tokens: u64,
    /// Tokens-per-minute consumption in the current rate window.
    pub tokens_per_min: f64,
    /// Configured TPM budget (0 = unlimited).
    pub tpm_limit: f64,
    /// Estimated cumulative cost in USD.
    pub cost_usd: f64,
    /// Container CPU usage (0–100).
    pub cpu_pct: f64,
    /// Container memory usage in MiB.
    pub mem_mb: u64,
    /// Number of messages in the current conversation history.
    pub message_count: usize,
    /// Terminal column count — used to right-pad the bar to fill the row.
    pub width: u16,
}

impl Default for BarData {
    fn default() -> Self {
        BarData {
            model: String::from("claude-opus-4-6"),
            input_tokens: 0,
            output_tokens: 0,
            tokens_per_min: 0.0,
            tpm_limit: 0.0,
            cost_usd: 0.0,
            cpu_pct: 0.0,
            mem_mb: 0,
            message_count: 0,
            width: 80,
        }
    }
}

/// Format a token count as "123K" when >= 1000, otherwise as the raw number.
fn fmt_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}K", n / 1_000)
    } else {
        format!("{}", n)
    }
}

/// Right-pad `s` with spaces to reach visible width `w`.
///
/// ANSI escape sequences are not counted toward visible width; this function
/// strips them before computing the visible length.
pub fn pad_to(s: &str, w: usize) -> String {
    let visible = ansi_visible_len(s);
    if visible >= w {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(w - visible))
    }
}

/// Count visible (non-ANSI) characters in a string.
pub fn ansi_visible_len(s: &str) -> usize {
    let mut len = 0usize;
    let mut in_escape = false;
    for ch in s.chars() {
        if ch == '\x1b' {
            in_escape = true;
        } else if in_escape {
            if ch.is_alphabetic() {
                in_escape = false;
            }
        } else {
            len += 1;
        }
    }
    len
}

/// Build the top status bar: branding + token counts + TPM bar + cost + container stats + message count.
///
/// Layout: `Syl ─ ↑ N  ↓ N  · $N.NNNN ─── TPMbar pct% ─── cpu% mem ─── N msgs ──────`
/// The one status bar: brand and model, then the meters, then `/help`.
///
/// ```text
///  Syl · claude-opus-5-5 ── ↑12K ↓1K ── $0.1234 ── ███░░░ 50% ── cpu 12% 256M ── 7 msgs ───── /help
/// ```
///
/// When the terminal is too narrow, segments are dropped, least useful
/// first: messages, container, TPM, tokens. The model and cost always stay.
pub fn make_bar(data: &BarData, p: &Palette) -> String {
    let w = data.width as usize;
    let (r, dim) = (p.r, p.dim);

    let head = format!(" {lb}Syl{r} {dim}·{r} {lb}{}{r}", data.model, lb = p.lb);
    let tokens = format!(
        "{cy}↑{r}{wh}{}{r} {cy}↓{r}{wh}{}{r}",
        fmt_k(data.input_tokens),
        fmt_k(data.output_tokens),
        cy = p.cy,
        wh = p.wh,
    );
    let cost = format!("{ye}${:.4}{r}", data.cost_usd, ye = p.ye);
    let tpm = (data.tpm_limit > 0.0).then(|| {
        let ratio = (data.tokens_per_min / data.tpm_limit).clamp(0.0, 1.0);
        let c = if ratio > 0.8 { p.ur } else if ratio > 0.5 { p.uy } else { p.ug };
        let filled = (ratio * 6.0).round() as usize;
        format!("{c}{}{}{r} {c}{:.0}%{r}", "█".repeat(filled), "░".repeat(6 - filled), ratio * 100.0)
    });
    let cpu_c = if data.cpu_pct >= 85.0 { p.ur } else if data.cpu_pct >= 60.0 { p.uy } else { p.ug };
    let cpu = format!("{dim}cpu{r} {cpu_c}{:.0}%{r} {dim}{}M{r}", data.cpu_pct, data.mem_mb);
    let msgs = format!("{dim}{} msgs{r}", data.message_count);
    let help = format!("{dim}/help ─{r}");

    // Optional segments, most important first; drop from the end to fit.
    let mut optional: Vec<String> = vec![tokens];
    optional.extend(tpm);
    optional.push(cpu);
    optional.push(msgs);

    let sep = format!(" {dim}──{r} ");
    loop {
        let mut parts = vec![head.clone()];
        parts.push(cost.clone());
        parts.extend(optional.iter().cloned());
        // Cost reads best right after the tokens it pays for.
        if parts.len() > 2 {
            parts.swap(1, 2);
        }
        let core = parts.join(&sep);
        let used = ansi_visible_len(&core) + 1 + ansi_visible_len(&help);
        if used < w || optional.is_empty() {
            let fill = w.saturating_sub(used + 1);
            return format!("{core} {dim}{}{r} {help}", "─".repeat(fill));
        }
        optional.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_k_small() {
        assert_eq!(fmt_k(0), "0");
        assert_eq!(fmt_k(999), "999");
    }

    #[test]
    fn fmt_k_kilo() {
        assert_eq!(fmt_k(1_000), "1K");
        assert_eq!(fmt_k(12_345), "12K");
    }

    #[test]
    fn fmt_k_mega() {
        assert!(fmt_k(1_000_000).contains('M'));
    }

    #[test]
    fn ansi_len_plain() {
        assert_eq!(ansi_visible_len("hello"), 5);
    }

    #[test]
    fn ansi_len_with_codes() {
        // "\x1b[96m" + "hi" + "\x1b[0m" — only "hi" is visible
        assert_eq!(ansi_visible_len("\x1b[96mhi\x1b[0m"), 2);
    }
}
