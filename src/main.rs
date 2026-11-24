//! Syl's terminal client. It owns the screen and the keyboard; the agent loop
//! runs in `syl-core` and is driven by `AgentCommand`s and observed through
//! `AgentEvent`s.
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;

use syl_container::Container;
use syl_core::agent::{self, Agent, AgentCommand, AgentEvent, Mode, SessionPaths, SessionStart, TurnEnd};
use syl_core::checkpoint::Checkpoints;
use syl_core::control::PermissionAnswer;
use syl_core::api::types::StopReason;
use syl_core::config::{build_config, parse_cli_args, AppConfig};
use syl_core::model::AnthropicAdapter;
use syl_core::session::{find_session, one_line, SessionInfo};
use syl_core::shared::SharedState;
use syl_rs::slash::{handle_slash, SlashAction, SlashContext};
use syl_rs::tui::{
    animation,
    bars::{make_bar, BarData, DARK, LIGHT},
    image_render,
    input::{Hotkey, InputEditor},
    spinner,
    state::TuiState,
    steer::{carry_over, KeyReader, SteerKey, SteerLine},
    terminal,
};

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Parse CLI args (hand-rolled parser in config.rs)
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let arg_strs: Vec<&str> = raw_args.iter().map(|s| s.as_str()).collect();
    let cli = parse_cli_args(&arg_strs);

    if cli.help {
        print_help();
        return Ok(());
    }

    // 2. Determine directories
    let project_dir = std::env::current_dir()?;
    let syl_dir = project_dir.join(".syl");
    std::fs::create_dir_all(&syl_dir)?;
    let paths = SessionPaths::for_project(&project_dir);
    let sessions_dir = paths.sessions_dir.clone().expect("standard layout saves sessions");

    // 3. Init logging (keep guard alive until process exit)
    let _log_guard = syl_core::logging::init_logging(&syl_dir)?;

    // 4. Build config
    let config = build_config(&cli, &syl_dir);
    if let Some(e) = &config.config_error {
        // Shown before anything else: with config.toml unreadable, its
        // permission rules aren't in force.
        eprintln!("[Syl] Could not read the config file, so its settings (permission rules included) are not in effect:\n  {e}");
    }

    // 5. Build reqwest client
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(config.timeout_ms))
        .build()?;

    // 6. Shared state: the core's, and the terminal's own
    let state = SharedState::new();
    *state.current_model.lock().unwrap() = config.model.clone();
    let tui = TuiState::new();

    // 6b. Which session to start in: resolved before the slow container
    // start so a bad --resume fails fast.
    let start = match &cli.resume {
        Some(query) if !cli.fresh => match find_session(&sessions_dir, query) {
            Ok(id) => SessionStart::Resume(id),
            Err(_) if query.is_empty() => {
                println!("[Syl] No saved session to continue; starting a new one.");
                SessionStart::New
            }
            Err(e) => anyhow::bail!("--resume: {e}"),
        },
        _ => SessionStart::New,
    };

    // 7. Container lifecycle
    let defaults = syl_container::ContainerOptions::default();
    let container = Arc::new(Container::for_project(&project_dir).with_options(syl_container::ContainerOptions {
        network: config.container.network.clone(),
        memory: config.container.memory.clone().unwrap_or(defaults.memory),
        cpus: config.container.cpus.clone(),
    }));
    if cli.fresh {
        container.remove(false);
    }
    container.start_or_resume(config.fresh, &|msg| println!("{msg}"))?;

    // 9. Enter raw mode
    crossterm::terminal::enable_raw_mode()?;

    // 10. Terminal setup: scroll region + initial bars
    let (cols, rows) = crossterm::terminal::size()?;
    {
        let mut out = tui.stdout.lock().unwrap();
        terminal::setup_scroll_region(&mut *out, rows)?;
    }
    draw_bars(&tui, &state, &config, &config.model, Mode::Build, cols, rows);

    // 10b. Startup banner
    if !config.no_banner {
        print_banner(&tui, cols, rows);
    }

    // 11. Fetch available models (5 s timeout; static fallback already in state)
    {
        let models = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            syl_core::api::models::fetch_models(&http, &config),
        )
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("model list fetch timed out, using static fallback");
            syl_core::api::models::static_fallback()
        });

        let n = models.len();
        *state.available_models.lock().unwrap() = models;
        let _ = terminal::print_to_scroll(
            &tui.stdout,
            &format!("\x1b[2m{n} models available  ·  /model to switch\x1b[0m\r\n"),
            rows,
        );
    }

    // 12. Start the agent
    let model = Arc::new(AnthropicAdapter::new(http.clone(), state.clone()));
    let mut handle = agent::spawn(Agent {
        config: config.clone(),
        state: state.clone(),
        model,
        container: container.clone(),
        paths,
        start,
    });

    // 13. Signals and background polling
    let resize_rx = terminal::spawn_sigwinch_handler();
    {
        let commands = handle.commands.clone();
        tokio::spawn(async move {
            loop {
                tokio::signal::ctrl_c().await.expect("ctrl-c handler failed");
                // Ignored by the agent when no turn is running.
                let _ = commands.send(AgentCommand::Cancel);
            }
        });
    }
    {
        let tui = tui.clone();
        let container = container.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                if let Some(stats) = container.stats() {
                    *tui.container_stats.lock().unwrap() = Some(stats);
                }
            }
        });
    }

    // 14. REPL
    let mut repl = Repl {
        config: &config,
        state: state.clone(),
        tui: tui.clone(),
        container: container.clone(),
        commands: handle.commands.clone(),
        model: config.model.clone(),
        rows,
        cols,
        syl_dir: &syl_dir,
        sessions_dir: &sessions_dir,
        session: None,
        carry: String::new(),
        last_tools: Vec::new(),
        mode: Mode::Build,
    };
    let result = match repl.await_session_reply(&mut handle.events).await {
        true => repl.run(&mut handle.events, resize_rx).await,
        false => Err(anyhow::anyhow!("the agent stopped unexpectedly")),
    };

    // 15. Teardown: stop the agent, disable raw mode, reset DECSTBM
    // The Ctrl+C task holds a sender clone, so dropping ours isn't enough
    // to end the loop: ask it to stop, and don't wait on it forever.
    drop(repl);
    let _ = handle.commands.send(AgentCommand::Shutdown);
    if tokio::time::timeout(std::time::Duration::from_secs(5), &mut handle.task)
        .await
        .is_err()
    {
        tracing::warn!("agent did not stop within 5s; aborting it");
        handle.task.abort();
    }
    crossterm::terminal::disable_raw_mode()?;
    {
        let mut out = tui.stdout.lock().unwrap();
        write!(out, "\x1b[r")?; // reset scroll region to full terminal
        let _ = out.flush();
    }

    // 16. Remove the agent container on clean exit
    println!("[Syl] Cleaning up container...");
    container.remove(false);

    result
}

fn print_help() {
    println!(
        "syl — AI coding assistant\n\n\
         Usage: syl [options]\n\n\
         Options:\n  \
         --model <name>      Override model (default: claude-opus-5-5)\n  \
         --rate-limit <n>    TPM budget  (default: 60000)\n  \
         --timeout <secs>    Request timeout in seconds  (default: 120)\n  \
         --thinking <n>      Thinking budget in tokens\n  \
         --image <mode>      auto | kitty | iterm2 | sixel | symbols\n  \
         --continue, -c      Continue the most recent session\n  \
         --resume [id]       Resume a session by id, id prefix, number or name\n  \
                             (the most recent with no id)\n  \
         --fresh             Nuke container + start a new session\n  \
         --no-banner         Suppress startup banner\n  \
         --light             Light colour theme\n  \
         --help              Show this help"
    );
}

fn print_banner(tui: &TuiState, cols: u16, rows: u16) {
    const BANNER: &str = include_str!("banner.ansi");
    const IMAGE_COLS: u16 = 135;

    let img_pad = " ".repeat((cols.saturating_sub(IMAGE_COLS) / 2) as usize);
    let center = |text: &str, vis: u16| -> String {
        let p = (cols.saturating_sub(vis) / 2) as usize;
        format!("{}{}", " ".repeat(p), text)
    };

    // 3 text lines appended below the image
    let text_block = [
        String::new(),
        center("\x1b[2mLife before death  \u{00b7}  Strength before weakness  \u{00b7}  Journey before destination\x1b[0m", 76),
        String::new(),
    ];

    // Scroll region height = rows - 3; show only as many image lines as fit
    let scroll_height = rows.saturating_sub(3) as usize;
    let image_available = scroll_height.saturating_sub(text_block.len());
    let all_image: Vec<&str> = BANNER.lines().collect();
    let image_lines = &all_image[..image_available.min(all_image.len())];

    let mut lines: Vec<String> = image_lines.iter().map(|l| format!("{img_pad}{l}")).collect();
    lines.extend(text_block);

    let _ = terminal::print_to_scroll(&tui.stdout, &lines.join("\n"), rows);
}

// ---------------------------------------------------------------------------
// Status bars
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn draw_bars(tui: &TuiState, state: &SharedState, config: &AppConfig, model: &str, mode: Mode, cols: u16, rows: u16) {
    let palette = if config.light { &LIGHT } else { &DARK };

    let (input_tokens, output_tokens, cost_usd) = {
        let tok = state.session_tokens.lock().unwrap();
        let cost = syl_core::pricing::compute_cost_usd(&tok, model);
        (tok.input_tokens, tok.output_tokens, cost)
    };

    let (cpu_pct, mem_mb) = match tui.container_stats.lock().unwrap().as_ref() {
        Some(s) => (s.cpu_percent, s.mem_usage_mb as u64),
        None => (0.0, 0),
    };

    let tokens_per_min = {
        let mut calls = state.recent_calls.lock().unwrap();
        syl_core::api::rate_limit::prune_old_calls(&mut calls);
        syl_core::api::rate_limit::window_tokens(&calls) as f64
    };

    let data = BarData {
        model: model.to_string(),
        input_tokens,
        output_tokens,
        tokens_per_min,
        tpm_limit: config.rate_limit as f64,
        cost_usd,
        cpu_pct,
        mem_mb,
        message_count: state.messages.lock().unwrap().len(),
        width: cols,
        plan_mode: mode == Mode::Plan,
    };

    let bar = make_bar(&data, palette);
    let mut out = tui.stdout.lock().unwrap();
    let _ = terminal::draw_status_bar(&mut *out, &bar, rows);
}

// ---------------------------------------------------------------------------
// REPL
// ---------------------------------------------------------------------------

struct Repl<'a> {
    config: &'a AppConfig,
    state: Arc<SharedState>,
    tui: Arc<TuiState>,
    container: Arc<Container>,
    commands: mpsc::UnboundedSender<AgentCommand>,
    /// Model shown in the bars and used for the next turn. Only the REPL
    /// changes it, so it never lags behind a `SetModel` the agent hasn't
    /// processed yet.
    model: String,
    rows: u16,
    cols: u16,
    syl_dir: &'a Path,
    sessions_dir: &'a Path,
    /// The current session, as the agent last reported it.
    session: Option<SessionInfo>,
    /// Text typed at the end of a turn, to start the next prompt with.
    carry: String,
    /// The last turn's tool calls (name, input, output, is_error), for /tools.
    last_tools: Vec<(String, serde_json::Value, String, bool)>,
    /// Plan or build, as last sent to the agent.
    mode: Mode,
}

impl Repl<'_> {
    fn print(&self, text: &str) {
        let _ = terminal::print_to_scroll(&self.tui.stdout, text, self.rows);
    }

    fn redraw_bars(&self) {
        draw_bars(&self.tui, &self.state, self.config, &self.model, self.mode, self.cols, self.rows);
    }

    /// Show the agent's answer to a session command: wait for the
    /// `SessionChanged` or `Error` it sends. Returns false if the agent went
    /// away.
    async fn await_session_reply(&mut self, events: &mut mpsc::UnboundedReceiver<AgentEvent>) -> bool {
        loop {
            match events.recv().await {
                None => return false,
                Some(AgentEvent::SessionChanged { info }) => {
                    self.show_session(&info);
                    self.session = Some(info);
                    return true;
                }
                Some(AgentEvent::Error { message }) => {
                    self.print(&format!("[Error: {message}]\r\n"));
                    // A failed resume at startup is followed by the new
                    // session the agent fell back to.
                    if self.session.is_some() {
                        return true;
                    }
                }
                Some(AgentEvent::Notice { message }) => self.print(&format!("{}\r\n", sanitize(&message))),
                Some(_) => {}
            }
        }
    }

    /// Switch plan/build and wait for the agent to confirm.
    async fn set_mode(&mut self, mode: Mode, events: &mut mpsc::UnboundedReceiver<AgentEvent>) -> bool {
        let wanted = mode;
        let _ = self.commands.send(AgentCommand::SetMode(mode));
        loop {
            match events.recv().await {
                None => return false,
                // A ModeChanged for an earlier switch (made during a turn and
                // applied when it ended) can come first: note it, keep waiting.
                Some(AgentEvent::ModeChanged { mode }) if mode != wanted => self.mode = mode,
                Some(AgentEvent::ModeChanged { mode }) => {
                    self.mode = mode;
                    let text = match mode {
                        Mode::Plan => "\x1b[33m[plan mode: Syl can read and run commands, but not change files · Shift+Tab or /build to switch back]\x1b[0m",
                        Mode::Build => "\x1b[2m[build mode: file changes allowed]\x1b[0m",
                    };
                    self.print(&format!("{text}\r\n"));
                    self.redraw_bars();
                    return true;
                }
                Some(AgentEvent::Notice { message }) => self.print(&format!("{}\r\n", sanitize(&message))),
                Some(_) => {}
            }
        }
    }

    /// This session's checkpoints, newest first.
    fn checkpoints(&self) -> Vec<(String, String, u64)> {
        let mut list = self
            .session
            .as_ref()
            .and_then(|s| syl_core::session::load_entries(self.sessions_dir, &s.id).ok())
            .map(|e| syl_core::session::checkpoints(&e))
            .unwrap_or_default();
        list.reverse();
        list
    }

    fn print_checkpoints(&self) {
        let list = self.checkpoints();
        if list.is_empty() {
            self.print("No checkpoints yet: one is taken before each turn that changes files.\r\n");
            return;
        }
        for (i, (commit, label, ts)) in list.iter().enumerate().take(30) {
            self.print(&format!(
                "{:>3}  {}  \x1b[2m{}\x1b[0m  {}\r\n",
                i + 1,
                syl_core::session::format_utc(*ts),
                syl_core::checkpoint::short(commit),
                sanitize(&one_line(label, 70))
            ));
        }
        self.print("\x1b[2m/restore <n> puts the files back as they were at that checkpoint.\x1b[0m\r\n");
    }

    /// `/undo` (n = None: the newest checkpoint) and `/restore n`: show what
    /// would change, confirm, then ask the agent to restore.
    async fn undo(
        &mut self,
        n: Option<usize>,
        editor: &mut InputEditor,
        events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    ) -> bool {
        let list = self.checkpoints();
        let Some((commit, label, _)) = list.get(n.unwrap_or(1) - 1).cloned() else {
            self.print(match n {
                None => "Nothing to undo: no turn has changed files yet.\r\n",
                Some(_) => "No such checkpoint (see /checkpoints).\r\n",
            });
            return true;
        };
        let project = self.syl_dir.parent().unwrap_or(self.syl_dir).to_path_buf();
        let preview = tokio::task::block_in_place(|| Checkpoints::for_project(&project).preview(&commit));
        let changes = match preview {
            Ok(c) => c,
            Err(e) => {
                self.print(&format!("[Error: {e}]\r\n"));
                return true;
            }
        };
        if changes.is_empty() {
            self.print(&format!(
                "Nothing has changed since the checkpoint {}{}.\r\n",
                sanitize(&one_line(&label, 60)),
                if n.is_none() { " (to go further back: /checkpoints, then /restore <n>)" } else { "" }
            ));
            return true;
        }
        self.print(&format!(
            "Restore {} files to how they were {}:\r\n",
            changes.len(),
            sanitize(&one_line(&label, 70))
        ));
        for c in changes.iter().take(20) {
            let (mark, what) = match c.status {
                'A' => ("\x1b[32m+\x1b[0m", "comes back"),
                'D' => ("\x1b[31m-\x1b[0m", "is removed"),
                _ => ("\x1b[33m~\x1b[0m", "goes back"),
            };
            self.print(&format!("  {mark} {}  \x1b[2m{what}\x1b[0m\r\n", sanitize(&c.path)));
        }
        if changes.len() > 20 {
            self.print(&format!("  \x1b[2m… and {} more\x1b[0m\r\n", changes.len() - 20));
        }
        let answer = editor.readline("Restore? [y/N] ", self.rows, "").unwrap_or_default();
        let _ = editor.take_hotkey();
        if !answer.trim().eq_ignore_ascii_case("y") {
            self.print("\x1b[2m[not restored]\x1b[0m\r\n");
            return true;
        }
        let _ = self.commands.send(AgentCommand::Restore { commit });
        loop {
            match events.recv().await {
                None => return false,
                Some(AgentEvent::Restored { files, .. }) => {
                    self.print(&format!(
                        "\x1b[32m[restored {files} files · the restore can itself be undone with /undo]\x1b[0m\r\n"
                    ));
                    return true;
                }
                Some(AgentEvent::Error { message }) => {
                    self.print(&format!("[Error: {message}]\r\n"));
                    return true;
                }
                Some(AgentEvent::Notice { message }) => self.print(&format!("{}\r\n", sanitize(&message))),
                Some(_) => {}
            }
        }
    }

    /// `/receipts`: what the last `n` turns did.
    fn print_receipts(&self, n: usize) {
        let receipts = self
            .session
            .as_ref()
            .and_then(|s| syl_core::session::load_entries(self.sessions_dir, &s.id).ok())
            .map(|e| syl_core::session::receipts(&e))
            .unwrap_or_default();
        if receipts.is_empty() {
            self.print("No turns yet.\r\n");
            return;
        }
        let start = receipts.len().saturating_sub(n);
        for (i, r) in receipts.iter().enumerate().skip(start) {
            self.print(&format!(
                "\x1b[1m#{} \x1b[0m{}  \x1b[2m{}\x1b[0m\r\n",
                i + 1,
                sanitize(&one_line(&r.prompt, 70)),
                syl_core::session::format_utc(r.ts)
            ));
            let line = |label: &str, items: &[String]| {
                if !items.is_empty() {
                    let shown: Vec<String> = items.iter().take(8).map(|i| sanitize(&one_line(i, 80))).collect();
                    let more = if items.len() > 8 { format!(" … +{}", items.len() - 8) } else { String::new() };
                    self.print(&format!("    \x1b[2m{label:<9}\x1b[0m {}{more}\r\n", shown.join(" · ")));
                }
            };
            line("edited", &r.files);
            line("ran", &r.commands);
            line("fetched", &r.urls);
            line("approvals", &r.approvals);
            if r.errors > 0 {
                self.print(&format!("    \x1b[2m{:<9}\x1b[0m {}\r\n", "failed", r.errors));
            }
            if let Some(c) = &r.checkpoint {
                self.print(&format!("    \x1b[2m{:<9} {}\x1b[0m\r\n", "checkpoint", syl_core::checkpoint::short(c)));
            }
        }
    }

    fn show_session(&self, info: &SessionInfo) {
        let name = info.name.as_deref().map(|n| format!(" \"{}\"", one_line(n, 60))).unwrap_or_default();
        let line = if self.session.as_ref().is_some_and(|s| s.id == info.id) {
            format!("Session {}{name}", info.id)
        } else if info.resumed {
            format!("Resumed session {}{name} ({} messages)", info.id, info.message_count)
        } else if let Some(from) = &info.forked_from {
            format!("Forked {from} into session {}{name} ({} messages)", info.id, info.message_count)
        } else if self.session.is_some() {
            format!("New session {}", info.id)
        } else {
            return;
        };
        self.print(&format!("\x1b[2m[{line}]\x1b[0m\r\n"));
    }

    /// The spinner on the status row, naming the tool calls running now.
    fn show_running(&self, active: &[(String, String, serde_json::Value)]) {
        self.tui.stop_spinner();
        let Some((_, name, input)) = active.last() else { return };
        let more = if active.len() > 1 { format!("  (+{} more)", active.len() - 1) } else { String::new() };
        // Room for the spinner frame and the elapsed seconds around it.
        let label = clip_visible(&format!("{}{more}", tool_brief(name, input)), self.cols.saturating_sub(12));
        let sp = spinner::spawn_spinner(self.tui.stdout.clone(), self.rows, label);
        *self.tui.spinner.lock().unwrap() = Some(sp);
    }

    /// `/tools`: the last turn's tool calls in full, read from the session
    /// log (so it works after `--resume` too), or from this run's memory
    /// when there is no log.
    fn print_last_tools(&self) {
        let logged: Option<Vec<(String, serde_json::Value, String, bool)>> = self
            .session
            .as_ref()
            .and_then(|s| syl_core::session::load_entries(self.sessions_dir, &s.id).ok())
            .map(|entries| {
                syl_core::session::last_turn_tool_calls(&entries)
                    .into_iter()
                    .map(|c| (c.name, c.input, c.output.unwrap_or_else(|| "(no result logged)".into()), c.is_error))
                    .collect()
            });
        let calls = logged.unwrap_or_else(|| self.last_tools.clone());
        if calls.is_empty() {
            self.print("No tool calls in the last turn.\r\n");
            return;
        }
        for (name, input, output, is_error) in &calls {
            let mark = if *is_error { "\x1b[31m✗\x1b[0m" } else { "\x1b[32m✓\x1b[0m" };
            self.print(&format!("{mark} \x1b[1m{}\x1b[0m\r\n", tool_brief(name, input)));
            self.print(&format!("{}\r\n", tool_display(name, input, output)));
        }
    }

    fn apply_resize(&mut self, cols: u16, rows: u16, turn_active: bool) {
        self.cols = cols;
        self.rows = rows;
        let _ = handle_resize(cols, rows, &self.tui, turn_active);
    }

    async fn run(
        &mut self,
        events: &mut mpsc::UnboundedReceiver<AgentEvent>,
        mut resize_rx: tokio::sync::watch::Receiver<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let mut editor = InputEditor::new()?;
        let workflow_dir = self.syl_dir.join("workflows");
        let memory_dir = self.syl_dir.join("memory");
        let log_path = self.syl_dir.join("syl.log");

        if let Some(e) = &self.config.config_error {
            self.print(&format!(
                "\x1b[31m[Syl] .syl/config.toml can't be read, so Syl will ask before every tool call until it's fixed:\x1b[0m\r\n  {}\r\n",
                sanitize(e)
            ));
        }
        for w in &self.config.config_warnings {
            self.print(&format!("\x1b[33m[Syl] {}\x1b[0m\r\n", sanitize(w)));
        }
        self.print("Type a message, or /help for commands.\r\n");

        loop {
            // Drain any pending resize before showing the prompt
            while resize_rx.has_changed().unwrap_or(false) {
                let (nc, nr) = *resize_rx.borrow_and_update();
                self.apply_resize(nc, nr, false);
            }
            self.redraw_bars();

            let initial = std::mem::take(&mut self.carry);
            let line = match editor.readline("> ", self.rows, &initial) {
                Ok(l) => l,
                Err(rustyline::error::ReadlineError::Eof) => break,
                // Ctrl+C clears the line; it often lands just after a turn it
                // was meant to cancel. Ctrl+D or /exit quits.
                Err(rustyline::error::ReadlineError::Interrupted) => continue,
                Err(e) => return Err(e.into()),
            };
            match editor.take_hotkey() {
                Some(Hotkey::ToggleMode) => {
                    self.carry = line;
                    let next = if self.mode == Mode::Plan { Mode::Build } else { Mode::Plan };
                    if !self.set_mode(next, events).await {
                        anyhow::bail!("the agent stopped unexpectedly");
                    }
                    continue;
                }
                Some(Hotkey::Undo) => {
                    self.carry = line;
                    if !self.undo(None, &mut editor, events).await {
                        anyhow::bail!("the agent stopped unexpectedly");
                    }
                    continue;
                }
                None => {}
            }
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            editor.add_history_entry(&line);

            // What you typed stays in the conversation, on the right like a
            // sent message (rustyline's own echo is on the input row, which
            // the next prompt overwrites).
            self.print(&format!("\r\n{}", chat_bubble(&sanitize(&line), self.cols, self.config.light)));

            if line == "/tools" {
                self.print_last_tools();
                continue;
            }
            let ok = match line.as_str() {
                "/plan" => Some(self.set_mode(Mode::Plan, events).await),
                "/build" => Some(self.set_mode(Mode::Build, events).await),
                "/mode" => {
                    let next = if self.mode == Mode::Plan { Mode::Build } else { Mode::Plan };
                    Some(self.set_mode(next, events).await)
                }
                "/undo" => Some(self.undo(None, &mut editor, events).await),
                "/checkpoints" => {
                    self.print_checkpoints();
                    Some(true)
                }
                l if l.starts_with("/restore") => {
                    let n = l.trim_start_matches("/restore").trim().parse::<usize>().ok();
                    match n {
                        Some(n) if n > 0 => Some(self.undo(Some(n), &mut editor, events).await),
                        _ => {
                            self.print("Usage: /restore <n>  (n from /checkpoints; 1 is the newest)\r\n");
                            Some(true)
                        }
                    }
                }
                l if l == "/receipts" || l.starts_with("/receipts ") => {
                    let n = l.trim_start_matches("/receipts").trim().parse::<usize>().unwrap_or(5);
                    self.print_receipts(n);
                    Some(true)
                }
                _ => None,
            };
            match ok {
                Some(true) => continue,
                Some(false) => anyhow::bail!("the agent stopped unexpectedly"),
                None => {}
            }

            if line.starts_with('/') {
                // The model picker needs the terminal directly, so it runs
                // before the generic dispatcher.
                if line == "/model" || line.starts_with("/model ") {
                    if let Some(new_model) = self.run_model_selector(&line).await {
                        let _ = self.commands.send(AgentCommand::SetModel(new_model.clone()));
                        self.model = new_model;
                        self.redraw_bars();
                    }
                    continue;
                }

                // Slash commands only run between turns, so editing history
                // here can't race the agent.
                let message_count = self.state.messages.lock().unwrap().len();
                let (input_tokens, output_tokens) = {
                    let tok = self.state.session_tokens.lock().unwrap();
                    (tok.input_tokens, tok.output_tokens)
                };
                let slash_ctx = SlashContext {
                    sessions_dir: self.sessions_dir,
                    session_id: self.session.as_ref().map(|s| s.id.as_str()),
                    workflow_dir: &workflow_dir,
                    memory_dir: &memory_dir,
                    log_path: &log_path,
                    model: &self.model,
                    timeout_ms: self.config.timeout_ms,
                    rate_limit: self.config.rate_limit,
                    input_tokens,
                    output_tokens,
                    message_count,
                    config: self.config,
                    container: &self.container,
                };
                match handle_slash(&line, &slash_ctx).await {
                    Some(result) => {
                        if let Some(text) = result.output {
                            self.print(&format!("{}\r\n", text.replace('\n', "\r\n")));
                        }
                        match result.action {
                            SlashAction::Exit => break,
                            SlashAction::Continue => {}
                            SlashAction::Agent(cmd) => {
                                let _ = self.commands.send(cmd);
                                if !self.await_session_reply(events).await {
                                    anyhow::bail!("the agent stopped unexpectedly");
                                }
                            }
                        }
                    }
                    None => self.print(&format!("Unknown command: {line}\r\n")),
                }
                continue;
            }

            let project_dir = self.syl_dir.parent().unwrap_or(self.syl_dir);
            let attached = syl_core::attachments::attach_images(&line, project_dir);
            for problem in &attached.problems {
                self.print(&format!("\x1b[33m[image not attached: {problem}]\x1b[0m\r\n"));
            }
            let cmd = if attached.images.is_empty() {
                AgentCommand::Prompt(line)
            } else {
                self.print(&format!("\x1b[2m[{} image(s) attached]\x1b[0m\r\n", attached.images.len()));
                AgentCommand::PromptWithImages { text: attached.text, images: attached.images }
            };
            let _ = self.commands.send(cmd);
            if !self.drive_turn(events, &mut resize_rx).await {
                anyhow::bail!("the agent stopped unexpectedly");
            }
        }

        Ok(())
    }

    /// Render one turn's events until `TurnEnded`. Returns false if the
    /// agent went away.
    async fn drive_turn(
        &mut self,
        events: &mut mpsc::UnboundedReceiver<AgentEvent>,
        resize_rx: &mut tokio::sync::watch::Receiver<(u16, u16)>,
    ) -> bool {
        let mut streaming = false;
        let mut inputs: HashMap<String, serde_json::Value> = HashMap::new();
        // Tool calls running now, in the order they started, for the ticker.
        let mut active: Vec<(String, String, serde_json::Value)> = Vec::new();
        self.last_tools.clear();
        let mut resize_open = true;
        let mut reader = KeyReader::start();
        let mut steer = SteerLine::default();
        // Steering sent and not yet answered. The agent answers each with
        // SteerDelivered, SteerQueued (a turn for it follows, rendered here
        // too) or SteerDropped, so this keeps reading until all are settled.
        let mut unanswered = 0usize;
        let mut turns_expected = 0usize;
        let mut in_turn = true;
        // A tool call waiting for y / a / n, and when it was asked: keys in
        // the first moment are typeahead, not answers.
        let mut asking: Option<(String, std::time::Instant)> = None;
        steer.draw(&self.tui.stdout, self.rows, self.cols);

        let ended = loop {
            let event = tokio::select! {
                ev = events.recv() => match ev {
                    Some(ev) => ev,
                    None => break false,
                },
                Some(key) = reader.keys.recv() => {
                    // While a permission question is open, keys answer it.
                    if let (Some((id, asked_at)), crossterm::event::Event::Key(k)) = (&asking, &key) {
                        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
                        if k.kind == KeyEventKind::Press && !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                            if asked_at.elapsed() < std::time::Duration::from_millis(300) {
                                continue;
                            }
                            let answer = match k.code {
                                KeyCode::Char('y' | 'Y') => Some((PermissionAnswer::Once, "allowed once")),
                                KeyCode::Char('a' | 'A') => Some((PermissionAnswer::Always, "allowed, and from now on in this project")),
                                KeyCode::Char('n' | 'N') | KeyCode::Esc => Some((PermissionAnswer::Reject, "declined")),
                                _ => None,
                            };
                            if let Some((answer, said)) = answer {
                                let _ = self.commands.send(AgentCommand::ApprovePermission { id: id.clone(), answer });
                                let _ = asked_at;
                                self.print(&format!("\x1b[2m  → {said}\x1b[0m\r\n"));
                                asking = None;
                                steer.draw(&self.tui.stdout, self.rows, self.cols);
                            }
                            continue;
                        }
                    }
                    match steer.handle_event(key) {
                        SteerKey::Edited => steer.draw(&self.tui.stdout, self.rows, self.cols),
                        SteerKey::Send(text) => {
                            if streaming {
                                let _ = terminal::end_streaming(&self.tui.stdout, self.rows);
                                streaming = false;
                            }
                            self.print(&format!("\x1b[2m» {}\x1b[0m\r\n", one_line(&text, 200)));
                            let _ = self.commands.send(AgentCommand::Steer(text));
                            unanswered += 1;
                            steer.draw(&self.tui.stdout, self.rows, self.cols);
                        }
                        SteerKey::Cancel => {
                            let _ = self.commands.send(AgentCommand::Cancel);
                            asking = None;
                            steer.draw(&self.tui.stdout, self.rows, self.cols);
                        }
                        SteerKey::ToggleMode => {
                            // The agent applies it when this turn ends; the bar
                            // shows it now.
                            self.mode = if self.mode == Mode::Plan { Mode::Build } else { Mode::Plan };
                            let _ = self.commands.send(AgentCommand::SetMode(self.mode));
                            self.print(&format!("\x1b[2m[{} mode from the next turn]\x1b[0m\r\n", self.mode.name()));
                            self.redraw_bars();
                        }
                        SteerKey::Ignored => {}
                    }
                    continue;
                },
                changed = resize_rx.changed(), if resize_open => {
                    if changed.is_err() {
                        resize_open = false;
                        continue;
                    }
                    let (nc, nr) = *resize_rx.borrow_and_update();
                    self.apply_resize(nc, nr, true);
                    // The resize stopped the spinner, which drew at the old
                    // row: start it again where the ticker row is now.
                    if !active.is_empty() {
                        self.tui.stop_anim();
                        self.show_running(&active);
                    }
                    steer.draw(&self.tui.stdout, nr, nc);
                    if streaming {
                        // Text is already flowing: no thinking animation, and
                        // re-anchor the streaming line inside the new region.
                        self.tui.stop_anim();
                        let _ = terminal::end_streaming(&self.tui.stdout, nr);
                        let _ = terminal::begin_streaming(&self.tui.stdout, nr);
                    }
                    continue;
                }
            };

            let is_delta = matches!(event, AgentEvent::TextDelta { .. } | AgentEvent::ThinkingDelta { .. });
            if streaming && !is_delta {
                let _ = terminal::end_streaming(&self.tui.stdout, self.rows);
                streaming = false;
            }

            match event {
                AgentEvent::TurnStarted => {
                    in_turn = true;
                    turns_expected = turns_expected.saturating_sub(1);
                }
                AgentEvent::RequestStarted => {
                    self.tui.stop_spinner();
                    let mut anim = self.tui.anim.lock().unwrap();
                    if anim.is_none() {
                        *anim = Some(animation::spawn_animation(self.tui.stdout.clone(), self.rows, self.cols));
                    }
                }
                AgentEvent::TextDelta { text } | AgentEvent::ThinkingDelta { text } => {
                    if !streaming {
                        streaming = true;
                        self.tui.stop_anim();
                        let _ = terminal::begin_streaming(&self.tui.stdout, self.rows);
                    }
                    let _ = terminal::write_streaming(&self.tui.stdout, &text);
                }
                AgentEvent::Usage { .. } => {
                    self.tui.stop_anim();
                    // Keep the meters current through a long turn.
                    self.redraw_bars();
                }
                AgentEvent::ToolCallStarted { id, name, input } => {
                    self.tui.stop_anim();
                    active.push((id.clone(), name, input.clone()));
                    self.show_running(&active);
                    inputs.insert(id, input);
                }
                AgentEvent::ToolCallFinished { id, name, output, is_error } => {
                    let input = inputs.remove(&id).unwrap_or_default();
                    active.retain(|(a, _, _)| *a != id);
                    if name == "show_image" && !is_error {
                        let path = input["path"].as_str().unwrap_or("");
                        if !image_render::render_image(path, &self.container, &self.tui, self.rows, self.cols) {
                            self.print(&format!("[show_image: could not render {path}]\r\n"));
                        }
                    }
                    // Tool calls go on the one-line ticker, not into the
                    // conversation; /tools shows them in full.
                    if active.is_empty() {
                        self.tui.stop_spinner();
                        let mark = if is_error { "\x1b[31m✗\x1b[0m" } else { "\x1b[32m✓\x1b[0m" };
                        let line = format!(
                            " {mark} {}  \x1b[2m{}\x1b[0m",
                            tool_brief(&name, &input),
                            one_line(&sanitize(&tool_outcome(&name, &output, is_error)), 60)
                        );
                        terminal::draw_ticker(&self.tui.stdout, self.rows, &clip_visible(&line, self.cols));
                    } else {
                        self.show_running(&active);
                    }
                    self.last_tools.push((name, input, output, is_error));
                }
                AgentEvent::PermissionRequested { id, name, input, subject, note } => {
                    self.tui.stop_anim();
                    self.tui.stop_spinner();
                    // The whole call, never cut: approving shows exactly what runs.
                    let full = match name.as_str() {
                        "run_bash" => input["command"].as_str().unwrap_or("").to_string(),
                        _ => serde_json::to_string(&input).unwrap_or_default(),
                    };
                    self.print(&format!("\x1b[33m?\x1b[0m Syl wants to use \x1b[1m{name}\x1b[0m:\r\n"));
                    for line in sanitize(&full).lines() {
                        self.print(&format!("    {line}\r\n"));
                    }
                    if !subject.is_empty() && subject.trim() != full.trim() {
                        self.print(&format!("  \x1b[2masking because of: {}\x1b[0m\r\n", sanitize(&subject)));
                    }
                    if let Some(note) = note {
                        self.print(&format!("  \x1b[2m{}\x1b[0m\r\n", sanitize(&one_line(&note, 200))));
                    }
                    self.print("  \x1b[1my\x1b[0m allow once · \x1b[1ma\x1b[0m always (this project) · \x1b[1mn\x1b[0m no\r\n");
                    terminal::draw_ticker(&self.tui.stdout, self.rows, " \x1b[33m? waiting for you: y / a / n\x1b[0m");
                    asking = Some((id, std::time::Instant::now()));
                }
                AgentEvent::PermissionDecided { .. } | AgentEvent::Checkpointed { .. } => {}
                AgentEvent::ModeChanged { mode } => {
                    self.mode = mode;
                    self.redraw_bars();
                }
                AgentEvent::Restored { files, .. } => {
                    self.print(&format!("\x1b[2m[restored {files} files]\x1b[0m\r\n"));
                }
                AgentEvent::Compacted { before, after } => {
                    tracing::info!(before, after, "history compacted");
                    self.print(&format!("\x1b[2m[context compacted: {before} → {after} messages]\x1b[0m\r\n"));
                }
                AgentEvent::SessionChanged { info } => self.session = Some(info),
                AgentEvent::SteerQueued { text, count } => {
                    unanswered = unanswered.saturating_sub(count);
                    turns_expected += 1;
                    self.print(&format!("\x1b[2m[sending as the next prompt: {}]\x1b[0m\r\n", one_line(&text, 70)));
                }
                AgentEvent::SteerDropped { text, count } => {
                    unanswered = unanswered.saturating_sub(count);
                    self.print(&format!("\x1b[33m[not delivered, the turn stopped: {}]\x1b[0m\r\n", one_line(&text, 70)));
                    if !in_turn && unanswered == 0 && turns_expected == 0 {
                        break true;
                    }
                }
                AgentEvent::SteerDelivered { text } => {
                    unanswered = unanswered.saturating_sub(1);
                    self.print(&format!("\x1b[2m[delivered: {}]\x1b[0m\r\n", one_line(&text, 70)))
                }
                AgentEvent::Notice { message } => self.print(&format!("{}\r\n", sanitize(&message))),
                AgentEvent::Error { message } => self.print(&format!("[Error: {message}]\r\n")),
                AgentEvent::TurnEnded { reason } => {
                    match reason {
                        TurnEnd::Completed(StopReason::MaxTokens) => {
                            self.print("\x1b[2m[response hit max_tokens and was cut off]\x1b[0m\r\n")
                        }
                        TurnEnd::Completed(StopReason::ContextWindowExceeded) => self.print(
                            "\x1b[2m[the context window filled up; it will be compacted before the next request]\x1b[0m\r\n",
                        ),
                        TurnEnd::Cancelled => self.print("[cancelled]\r\n"),
                        TurnEnd::Completed(_) | TurnEnd::Failed => {}
                    }
                    in_turn = false;
                    if unanswered > 0 || turns_expected > 0 {
                        continue;
                    }
                    break true;
                }
            }
        };

        self.tui.stop_anim();
        self.tui.stop_spinner();
        let left = reader.stop().await;
        self.carry = carry_over(&steer, left);
        SteerLine::clear(&self.tui.stdout, self.rows);
        ended
    }

    /// Handle `/model`.
    ///
    /// With no argument: the interactive arrow-key picker, populated from
    /// `state.available_models` (fetched from the API at startup).
    /// With an argument: exact or substring match against the model list; an
    /// unknown name is still accepted and the API validates it.
    async fn run_model_selector(&self, line: &str) -> Option<String> {
        let arg = line.strip_prefix("/model").unwrap_or("").trim().to_owned();

        if arg.is_empty() {
            let items: Vec<syl_rs::tui::picker::ModelOption> = {
                let models = self.state.available_models.lock().unwrap();
                models
                    .iter()
                    .map(|m| syl_rs::tui::picker::ModelOption {
                        id: m.id.clone(),
                        display: m.display_name.clone(),
                        // When display_name differs from id (dynamic API
                        // fetch), show the id dimmed on the right.
                        description: if m.id != m.display_name { m.id.clone() } else { String::new() },
                    })
                    .collect()
            };
            let stdout = self.tui.stdout.clone();
            let current = self.model.clone();
            let rows = self.rows;
            // block_in_place is safe: nothing else touches stdin/stdout now.
            tokio::task::block_in_place(move || {
                syl_rs::tui::picker::run_picker(&items, &current, &stdout, rows)
            })
        } else {
            let canonical: Option<String> = {
                let models = self.state.available_models.lock().unwrap();
                models
                    .iter()
                    .find(|m| {
                        m.id == arg
                            || m.id.contains(arg.as_str())
                            || m.display_name.to_lowercase().contains(&arg.to_lowercase())
                    })
                    .map(|m| m.id.clone())
            };

            let model_id = canonical.clone().unwrap_or_else(|| arg.clone());
            let note = if canonical.is_none() {
                " \x1b[2m(not in model list — the API will validate)\x1b[0m"
            } else {
                ""
            };
            self.print(&format!(
                "\x1b[2m[Switched to \x1b[0m\x1b[96m{model_id}\x1b[0m{note}\x1b[2m]\x1b[0m\r\n"
            ));
            Some(model_id)
        }
    }
}

/// The user's message as a right-aligned block with a tinted background,
/// the way a messaging app shows what you sent. Wrapped by words to about
/// two thirds of the width; widths are in terminal columns, so wide
/// characters line up.
fn chat_bubble(text: &str, cols: u16, light: bool) -> String {
    use unicode_width::UnicodeWidthStr;
    let cols = cols as usize;
    let max = (cols * 2 / 3).clamp(10, cols.saturating_sub(4).max(10));
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        let mut word = word.to_string();
        // Hard-break a word wider than the bubble.
        while word.width() > max {
            let mut cut = String::new();
            for c in word.chars() {
                if (cut.clone() + &c.to_string()).width() > max {
                    break;
                }
                cut.push(c);
            }
            word = word[cut.len()..].to_string();
            lines.push(cut);
        }
        match lines.last_mut() {
            Some(last) if !last.is_empty() && last.width() + 1 + word.width() <= max => {
                last.push(' ');
                last.push_str(&word);
            }
            _ => lines.push(word),
        }
    }
    let inner = lines.iter().map(|l| l.width()).max().unwrap_or(0);
    let (bg, fg) = if light { ("\x1b[48;5;254m", "\x1b[30m") } else { ("\x1b[48;5;238m", "\x1b[97m") };
    // One column of margin on the right; one of padding inside each side.
    let indent = cols.saturating_sub(inner + 2 + 1);
    lines
        .iter()
        .map(|l| {
            let pad = inner - l.width();
            format!("{}{bg}{fg} {l}{} \x1b[0m\r\n", " ".repeat(indent), " ".repeat(pad))
        })
        .collect()
}

/// A short description of a tool call for the ticker: the tool and the
/// one argument that says what it is doing.
fn tool_brief(name: &str, input: &serde_json::Value) -> String {
    let arg = |k: &str| input[k].as_str().unwrap_or("").to_string();
    let detail = match name {
        "run_bash" => arg("command"),
        "read_file" | "edit_file" | "write_file" | "show_image" => arg("path"),
        "ls" => {
            let p = arg("path");
            if p.is_empty() { ".".to_string() } else { p }
        }
        "grep" | "glob" => arg("pattern"),
        "memory_read" | "memory_write" => arg("key"),
        "workflow_write" | "workflow_delete" => arg("name"),
        "dispatch_agent" => arg("source"),
        _ => String::new(),
    };
    let detail = one_line(&sanitize(&detail), 80);
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} \x1b[2m›\x1b[0m {detail}")
    }
}

/// What a finished call came to, in a few words.
fn tool_outcome(name: &str, output: &str, is_error: bool) -> String {
    if is_error {
        return output.lines().find(|l| !l.trim().is_empty()).unwrap_or("failed").to_string();
    }
    match name {
        "edit_file" | "write_file" => {
            let added = output.lines().filter(|l| l.starts_with('+') && !l.starts_with("+++")).count();
            let removed = output.lines().filter(|l| l.starts_with('-') && !l.starts_with("---")).count();
            format!("+{added} -{removed}")
        }
        "grep" | "glob" if output.starts_with("No ") => "no matches".to_string(),
        "grep" | "glob" => format!("{} results", output.lines().filter(|l| !l.starts_with("[...")).count()),
        "read_file" => output.lines().next().unwrap_or("").trim_matches(['[', ']']).to_string(),
        _ => {
            let n = output.lines().count();
            if n > 1 { format!("{n} lines") } else { output.trim().to_string() }
        }
    }
}

/// Clip a line with colour codes to `cols` terminal columns (the codes
/// themselves take no room; wide characters take two).
fn clip_visible(text: &str, cols: u16) -> String {
    use unicode_width::UnicodeWidthChar;
    let max = cols.saturating_sub(1) as usize;
    let mut out = String::new();
    let mut visible = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for n in chars.by_ref() {
                out.push(n);
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        let w = c.width().unwrap_or(0);
        if visible + w > max {
            break;
        }
        out.push(c);
        visible += w;
    }
    out.push_str("\x1b[0m");
    out
}

/// `text` with control characters other than newline and tab shown as
/// visible symbols, so tool output (file contents, a diff, a file name)
/// can't send escape sequences to the terminal.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            '\x1b' => '␛',
            c if c.is_control() => '�',
            c => c,
        })
        .collect()
}

/// How a finished tool call is shown in the scroll region.
fn tool_display(name: &str, input: &serde_json::Value, content: &str) -> String {
    let content = &sanitize(content);
    let input = &sanitize_json(input);
    match name {
        "run_bash" => {
            // Bordered output clipped to 40 lines
            let lines: Vec<&str> = content.lines().collect();
            let omitted = lines.len().saturating_sub(40);
            let mut out = "┌─ bash ─────────────────────────────────\r\n".to_string();
            for l in lines.iter().take(40) {
                out.push_str(&format!("│ {l}\r\n"));
            }
            if omitted > 0 {
                out.push_str(&format!("│ ... ({omitted} more lines)\r\n"));
            }
            out.push_str("└────────────────────────────────────────");
            out
        }
        "memory_read" | "memory_write" => format!("[memory] {content}"),
        "workflow_write" | "workflow_delete" => format!("[workflow] {content}"),
        "read_file" => {
            let path = input["path"].as_str().unwrap_or("?");
            format!("[read_file: {path}] ({} chars)", content.len())
        }
        "edit_file" | "write_file" => color_diff(content),
        "grep" | "glob" => {
            let pattern = input["pattern"].as_str().unwrap_or("?");
            let n = content.lines().filter(|l| !l.starts_with("[...")).count();
            let found = if content.starts_with("No ") { "no matches".to_string() } else { format!("{n} lines") };
            format!("[{name}: {pattern}] ({found})")
        }
        "ls" => {
            let path = input["path"].as_str().unwrap_or(".");
            let n = content.lines().filter(|l| !l.starts_with("total ")).count();
            format!("[ls: {path}] ({n} entries)")
        }
        "dispatch_agent" => {
            let source = input["source"].as_str().unwrap_or("?");
            let task = input["task"].as_str().unwrap_or("?");
            format!("[dispatch_agent: {task} {source}] ({} chars)", content.len())
        }
        "show_image" => {
            let path = input["path"].as_str().unwrap_or("?");
            format!("[show_image: {path}]")
        }
        _ => content.to_string(),
    }
}

/// An edit's result: the summary line, then the diff in colour, clipped.
fn color_diff(content: &str) -> String {
    const MAX_LINES: usize = 60;
    let lines: Vec<&str> = content.lines().collect();
    let mut out = String::new();
    for (i, l) in lines.iter().take(MAX_LINES).enumerate() {
        let colored = if i == 0 {
            format!("\x1b[1m{l}\x1b[0m")
        } else if l.starts_with("+++") || l.starts_with("---") {
            format!("\x1b[2m{l}\x1b[0m")
        } else if l.starts_with('+') {
            format!("\x1b[32m{l}\x1b[0m")
        } else if l.starts_with('-') {
            format!("\x1b[31m{l}\x1b[0m")
        } else if l.starts_with("@@") {
            format!("\x1b[36m{l}\x1b[0m")
        } else {
            l.to_string()
        };
        out.push_str(&colored);
        out.push_str("\r\n");
    }
    if lines.len() > MAX_LINES {
        out.push_str(&format!("\x1b[2m... ({} more diff lines)\x1b[0m\r\n", lines.len() - MAX_LINES));
    }
    out.trim_end_matches("\r\n").to_string()
}

fn sanitize_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => serde_json::Value::String(sanitize(s)),
        serde_json::Value::Array(a) => a.iter().map(sanitize_json).collect(),
        serde_json::Value::Object(o) => o.iter().map(|(k, v)| (k.clone(), sanitize_json(v))).collect(),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Resize handling
// ---------------------------------------------------------------------------

fn handle_resize(new_cols: u16, new_rows: u16, tui: &TuiState, turn_active: bool) -> anyhow::Result<()> {
    // Stop stale animation/spinner (they hold old row numbers)
    tui.stop_anim();
    tui.stop_spinner();

    // Re-establish DECSTBM with new dimensions
    {
        let mut out = tui.stdout.lock().unwrap();
        terminal::setup_scroll_region(&mut *out, new_rows)?;
    }

    // Restart animation if a turn is active
    if turn_active {
        let anim = animation::spawn_animation(tui.stdout.clone(), new_rows, new_cols);
        *tui.anim.lock().unwrap() = Some(anim);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ticker_names_the_call_by_its_key_argument() {
        assert_eq!(tool_brief("run_bash", &json!({"command": "cargo test\n --all"})), "run_bash \x1b[2m›\x1b[0m cargo test --all");
        assert_eq!(tool_brief("ls", &json!({})), "ls \x1b[2m›\x1b[0m .");
        assert_eq!(tool_brief("unknown", &json!({})), "unknown");
        // Control characters in arguments can't reach the terminal.
        assert!(!tool_brief("read_file", &json!({"path": "a\x1b]0;x\x07"})).contains('\x07'));
    }

    #[test]
    fn ticker_outcomes_are_short() {
        let diff = "Edited /w/a.rs (1 replacement).\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n-x\n+y\n+z\n";
        assert_eq!(tool_outcome("edit_file", diff, false), "+2 -1");
        assert_eq!(tool_outcome("grep", "No matches.", false), "no matches");
        assert_eq!(tool_outcome("run_bash", "\n[Tool error: boom]\nmore", true), "[Tool error: boom]");
    }

    #[test]
    fn clipping_counts_visible_characters_and_keeps_escapes_whole() {
        let line = "\x1b[32m✓\x1b[0m abcdefghij";
        let clipped = clip_visible(line, 6);
        assert_eq!(clipped, "\x1b[32m✓\x1b[0m abc\x1b[0m");
    }
}
