/// Status bar rendering: top (token telemetry) and bottom (model/spinner).
///
/// All functions are pure — no I/O.  The caller owns stdout and calls
/// `draw_status_bars` from `tui::terminal` to paint the returned strings.

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

/// All data needed to render both bars.
pub struct BarData {
    /// Model identifier shown in the bottom bar.
    pub model: String,
    /// Request timeout in milliseconds.
    pub timeout_ms: u64,
    /// Cumulative input tokens for this session.
    pub input_tokens: u64,
    /// Cumulative output tokens for this session.
    pub output_tokens: u64,
    /// Cumulative cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cumulative cache-write tokens.
    pub cache_write_tokens: u64,
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
    /// Terminal column count — used to right-pad bars to fill the row.
    pub width: u16,
    /// When `Some`, the bottom bar shows the spinner label rather than model info.
    pub spinner_label: Option<String>,
}

impl Default for BarData {
    fn default() -> Self {
        BarData {
            model: String::from("claude-opus-4-6"),
            timeout_ms: 60_000,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            tokens_per_min: 0.0,
            tpm_limit: 0.0,
            cost_usd: 0.0,
            cpu_pct: 0.0,
            mem_mb: 0,
            message_count: 0,
            width: 80,
            spinner_label: None,
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
pub fn make_top_bar(data: &BarData, p: &Palette) -> String {
    let w = data.width as usize;

    // Branding
    let brand = format!("{lb}Syl{r}", lb = p.lb, r = p.r);

    // Token counts — ↑ in  ↓ out  · cache_read/cache_write
    let tokens = format!(
        "{cy}↑{r}{wh}{inp}{r}  {cy}↓{r}{wh}{out}{r}  {dim}·{r} {dim}{cr}/{cw}{r}",
        cy  = p.cy,
        r   = p.r,
        wh  = p.wh,
        dim = p.dim,
        inp = fmt_k(data.input_tokens),
        out = fmt_k(data.output_tokens),
        cr  = fmt_k(data.cache_read_tokens),
        cw  = fmt_k(data.cache_write_tokens),
    );

    // Cost
    let cost = format!("{ye}${:.4}{r}", data.cost_usd, ye = p.ye, r = p.r);

    // TPM bar (6 blocks) + percentage
    let tpm_ratio = if data.tpm_limit > 0.0 {
        (data.tokens_per_min / data.tpm_limit).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let tpm_color = if tpm_ratio > 0.8 { p.ur } else if tpm_ratio > 0.5 { p.uy } else { p.ug };
    let tpm_seg = if data.tpm_limit > 0.0 {
        let filled = (tpm_ratio * 6.0).round() as usize;
        let bar: String = "█".repeat(filled) + &"░".repeat(6 - filled);
        format!("{c}{bar}{r} {c}{pct:.0}%{r}", c = tpm_color, r = p.r, bar = bar, pct = tpm_ratio * 100.0)
    } else {
        format!("{dim}──────{r}", dim = p.dim, r = p.r)
    };

    // CPU — colour-coded: green < 60%, yellow < 85%, red ≥ 85%
    let cpu_color = if data.cpu_pct >= 85.0 { p.ur } else if data.cpu_pct >= 60.0 { p.uy } else { p.ug };
    let cpu_seg = format!("{c}{cpu:.0}%{r} {dim}{mem}M{r}", c = cpu_color, r = p.r, dim = p.dim,
                          cpu = data.cpu_pct, mem = data.mem_mb);

    // Message count
    let msgs_seg = format!("{dim}{n} msgs{r}", n = data.message_count, dim = p.dim, r = p.r);

    // Assemble fixed segments separated by dim dash separators
    let sep = format!(" {dim}──{r} ", dim = p.dim, r = p.r);
    let core = format!(" {brand} {sep}{tokens}{sep}{cost}{sep}{tpm_seg}{sep}{cpu_seg}{sep}{msgs_seg} ",
                       brand = brand, sep = sep, tokens = tokens, cost = cost,
                       tpm_seg = tpm_seg, cpu_seg = cpu_seg, msgs_seg = msgs_seg);

    // Fill remaining width with dim dashes
    let visible = ansi_visible_len(&core);
    if visible < w {
        let fill = format!("{dim}{}{r}", "─".repeat(w - visible), dim = p.dim, r = p.r);
        format!("{}{}", core, fill)
    } else {
        core
    }
}

/// Build the bottom status bar: model + timeout + right-aligned shortcuts.
///
/// Layout: `─ model · Ns timeout ──────────────── /help  /save  /exit ─`
pub fn make_bottom_bar(data: &BarData, p: &Palette) -> String {
    let w = data.width as usize;

    let content = match &data.spinner_label {
        Some(label) => {
            // Spinner mode: just show the label, padded
            let s = format!(" {lb}{label}{r} ", lb = p.lb, r = p.r, label = label);
            return pad_to(&s, w);
        }
        None => {
            let timeout_s = data.timeout_ms / 1000;
            format!(
                "{dim}─{r} {lb}{model}{r} {dim}· {timeout}s{r}",
                dim     = p.dim,
                r       = p.r,
                lb      = p.lb,
                model   = data.model,
                timeout = timeout_s,
            )
        }
    };

    // Right-aligned shortcuts
    let shortcuts = format!("{dim}/help  /save  /exit ─{r}", dim = p.dim, r = p.r);
    let shortcuts_vis = ansi_visible_len(&shortcuts);

    let left_vis  = ansi_visible_len(&content);
    // +1 for leading space, +1 for space before shortcuts
    let total_fixed = 1 + left_vis + 1 + shortcuts_vis;

    let fill = if total_fixed < w {
        format!("{dim}{}{r}", "─".repeat(w - total_fixed), dim = p.dim, r = p.r)
    } else {
        String::new()
    };

    format!(" {content} {fill}{shortcuts}", content = content, fill = fill, shortcuts = shortcuts)
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
