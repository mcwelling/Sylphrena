//! Syl's terminal client. It owns the screen and the keyboard; the agent loop
//! runs in `syl-core` and is driven by `AgentCommand`s and observed through
//! `AgentEvent`s.
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::mpsc;

use syl_container::Container;
use syl_core::agent::{self, Agent, AgentCommand, AgentEvent, SessionPaths, TurnEnd};
use syl_core::api::types::StopReason;
use syl_core::config::{build_config, parse_cli_args, AppConfig};
use syl_core::model::AnthropicAdapter;
use syl_core::session::load_session;
use syl_core::shared::SharedState;
use syl_rs::slash::{handle_slash, SlashAction, SlashContext};
use syl_rs::tui::{
    animation,
    bars::{make_bottom_bar, make_top_bar, BarData, DARK, LIGHT},
    image_render,
    input::InputEditor,
    spinner,
    state::TuiState,
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
    let session_path = paths.session_path.clone().expect("standard layout has a session file");

    // 3. Init logging (keep guard alive until process exit)
    let _log_guard = syl_core::logging::init_logging(&syl_dir)?;

    // 4. Build config
    let config = build_config(&cli, &syl_dir);

    // 5. Build reqwest client
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(config.timeout_ms))
        .build()?;

    // 6. Shared state: the core's, and the terminal's own
    let state = SharedState::new();
    *state.current_model.lock().unwrap() = config.model.clone();
    let tui = TuiState::new();

    // 7. Container lifecycle
    let container = Arc::new(Container::for_project(&project_dir));
    if cli.fresh {
        container.remove(false);
    }
    container.start_or_resume(config.fresh, &|msg| println!("{msg}"))?;

    // 8. Load session on --resume (or if --fresh, start clean)
    if config.resume && !cli.fresh && session_path.exists() {
        if let Some(msgs) = load_session(&session_path) {
            println!("[Syl] Resuming previous session...");
            // The saved thinking blocks were bound to the old session's system
            // prompt; replaying them under a new one fails the prefix check.
            *state.messages.lock().unwrap() = syl_core::api::compaction::strip_thinking(msgs);
        }
    }

    // 9. Enter raw mode
    crossterm::terminal::enable_raw_mode()?;

    // 10. Terminal setup: scroll region + initial bars
    let (cols, rows) = crossterm::terminal::size()?;
    {
        let mut out = tui.stdout.lock().unwrap();
        terminal::setup_scroll_region(&mut *out, rows)?;
    }
    draw_bars(&tui, &state, &config, &config.model, cols, rows);

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
        session_path: &session_path,
    };
    let result = repl.run(&mut handle.events, resize_rx).await;

    // 15. Teardown: stop the agent, disable raw mode, reset DECSTBM
    drop(repl);
    drop(handle.commands);
    let _ = handle.task.await;
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
         --resume            Resume previous conversation session\n  \
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

    // Scroll region height = rows - 4; show only as many image lines as fit
    let scroll_height = rows.saturating_sub(4) as usize;
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

fn draw_bars(tui: &TuiState, state: &SharedState, config: &AppConfig, model: &str, cols: u16, rows: u16) {
    let palette = if config.light { &LIGHT } else { &DARK };

    let (input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd) = {
        let tok = state.session_tokens.lock().unwrap();
        let cost = syl_core::pricing::compute_cost_usd(&tok, model);
        (
            tok.input_tokens,
            tok.output_tokens,
            tok.cache_read_tokens,
            tok.cache_write_tokens,
            cost,
        )
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
        timeout_ms: config.timeout_ms,
        input_tokens,
        output_tokens,
        cache_read_tokens,
        cache_write_tokens,
        tokens_per_min,
        tpm_limit: config.rate_limit as f64,
        cost_usd,
        cpu_pct,
        mem_mb,
        message_count: state.messages.lock().unwrap().len(),
        width: cols,
        spinner_label: None,
    };

    let top = make_top_bar(&data, palette);
    let bottom = make_bottom_bar(&data, palette);

    let mut out = tui.stdout.lock().unwrap();
    let _ = terminal::draw_status_bars(&mut *out, &top, &bottom, rows);
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
    session_path: &'a Path,
}

impl Repl<'_> {
    fn print(&self, text: &str) {
        let _ = terminal::print_to_scroll(&self.tui.stdout, text, self.rows);
    }

    fn redraw_bars(&self) {
        draw_bars(&self.tui, &self.state, self.config, &self.model, self.cols, self.rows);
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

        self.print("Type a message, or /help for commands.\r\n");

        loop {
            // Drain any pending resize before showing the prompt
            while resize_rx.has_changed().unwrap_or(false) {
                let (nc, nr) = *resize_rx.borrow_and_update();
                self.apply_resize(nc, nr, false);
            }
            self.redraw_bars();

            let line = match editor.readline("> ", self.rows) {
                Ok(l) => l,
                Err(rustyline::error::ReadlineError::Eof)
                | Err(rustyline::error::ReadlineError::Interrupted) => break,
                Err(e) => return Err(e.into()),
            };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            editor.add_history_entry(&line);

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
                let msgs = self.state.messages.lock().unwrap().clone();
                let (input_tokens, output_tokens) = {
                    let tok = self.state.session_tokens.lock().unwrap();
                    (tok.input_tokens, tok.output_tokens)
                };
                let slash_ctx = SlashContext {
                    session_path: self.session_path,
                    workflow_dir: &workflow_dir,
                    memory_dir: &memory_dir,
                    log_path: &log_path,
                    model: &self.model,
                    timeout_ms: self.config.timeout_ms,
                    rate_limit: self.config.rate_limit,
                    input_tokens,
                    output_tokens,
                    message_count: msgs.len(),
                    config: self.config,
                    container: &self.container,
                };
                match handle_slash(&line, msgs, &slash_ctx).await {
                    Some(result) => {
                        *self.state.messages.lock().unwrap() = result.messages;
                        if let Some(text) = result.output {
                            self.print(&format!("{text}\r\n"));
                        }
                        if result.action == SlashAction::Exit {
                            break;
                        }
                    }
                    None => self.print(&format!("Unknown command: {line}\r\n")),
                }
                continue;
            }

            let _ = self.commands.send(AgentCommand::Prompt(line));
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
        let mut resize_open = true;

        let ended = loop {
            let event = tokio::select! {
                ev = events.recv() => match ev {
                    Some(ev) => ev,
                    None => break false,
                },
                changed = resize_rx.changed(), if resize_open => {
                    if changed.is_err() {
                        resize_open = false;
                        continue;
                    }
                    let (nc, nr) = *resize_rx.borrow_and_update();
                    self.apply_resize(nc, nr, true);
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
                AgentEvent::TurnStarted => {}
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
                AgentEvent::Usage { .. } => self.tui.stop_anim(),
                AgentEvent::ToolCallStarted { id, name, input } => {
                    self.tui.stop_anim();
                    self.tui.stop_spinner();
                    let sp = spinner::spawn_spinner(self.tui.stdout.clone(), self.rows, format!("{name}…"));
                    *self.tui.spinner.lock().unwrap() = Some(sp);
                    inputs.insert(id, input);
                }
                AgentEvent::ToolCallFinished { id, name, output, is_error } => {
                    let input = inputs.remove(&id).unwrap_or_default();
                    if name == "show_image" && !is_error {
                        let path = input["path"].as_str().unwrap_or("");
                        if !image_render::render_image(path, &self.container, &self.tui, self.rows, self.cols) {
                            self.print(&format!("[show_image: could not render {path}]\r\n"));
                        }
                    }
                    self.print(&format!("{}\r\n", tool_display(&name, &input, &output)));
                }
                AgentEvent::PermissionRequested { id, .. } => {
                    // Nothing asks yet (the default policy allows everything).
                    let _ = self.commands.send(AgentCommand::ApprovePermission { id, approved: true });
                }
                AgentEvent::Compacted { before, after } => {
                    tracing::info!(before, after, "history compacted");
                }
                AgentEvent::Notice { message } => self.print(&format!("{message}\r\n")),
                AgentEvent::Error { message } => self.print(&format!("[Error: {message}]\r\n")),
                AgentEvent::TurnEnded { reason } => {
                    match reason {
                        TurnEnd::Completed(StopReason::MaxTokens) => {
                            self.print("\x1b[2m[response hit max_tokens and was cut off]\x1b[0m\r\n")
                        }
                        TurnEnd::Cancelled => self.print("[cancelled]\r\n"),
                        TurnEnd::Completed(_) | TurnEnd::Failed => {}
                    }
                    break true;
                }
            }
        };

        self.tui.stop_anim();
        self.tui.stop_spinner();
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

/// How a finished tool call is shown in the scroll region.
fn tool_display(name: &str, input: &serde_json::Value, content: &str) -> String {
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
