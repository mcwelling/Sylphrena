/// Syl — main entry point.  Phase 4: full REPL wire-up.
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use syl_rs::api::client::{call_with_retry_shared, BlockDeltaEvent, CancelledError, SystemBlock};
use syl_rs::api::types::{ContentBlock, Message, MessageContent, StopReason};
use syl_rs::config::{build_config, parse_cli_args};
use syl_rs::container;
use syl_rs::session::{load_session, save_session};
use syl_rs::shared::SharedState;
use syl_rs::slash::{handle_slash, SlashAction, SlashContext};
use syl_rs::system_prompt::build_system_blocks;
use syl_rs::tools;
use syl_rs::tui::{
    animation,
    bars::{make_bottom_bar, make_top_bar, BarData, DARK, LIGHT},
    input::InputEditor,
    spinner, terminal,
};
use syl_rs::workspace::gather_workspace_context;

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
    let session_path = syl_dir.join("session.json");
    let memory_dir = syl_dir.join("memory");

    // 3. Init logging (keep guard alive until process exit)
    let _log_guard = syl_rs::logging::init_logging(&syl_dir)?;

    // 4. Build config
    let config = build_config(&cli, &syl_dir);

    // 5. Build reqwest client
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(config.timeout_ms))
        .build()?;

    // 6. Build SharedState
    let state = SharedState::new();
    *state.current_model.lock().unwrap() = config.model.clone();

    // 7. Container lifecycle
    if cli.fresh {
        container::nuke_container(false);
    }
    container::start_or_resume(&config, &syl_dir)?;

    // 8. Load session on --resume (or if --fresh, start clean)
    if config.resume && !cli.fresh && session_path.exists() {
        if let Some(msgs) = load_session(&session_path) {
            println!("[Syl] Resuming previous session...");
            // The saved thinking blocks were bound to the old session's system
            // prompt; replaying them under a new one fails the prefix check.
            *state.messages.lock().unwrap() =
                syl_rs::api::compaction::strip_thinking(msgs);
        }
    }

    // 9. Enter raw mode
    crossterm::terminal::enable_raw_mode()?;

    // 10. Terminal setup: scroll region + initial bars
    let (cols, rows) = crossterm::terminal::size()?;
    {
        let mut out = state.stdout.lock().unwrap();
        terminal::setup_scroll_region(&mut *out, rows)?;
    }
    draw_bars(&state, &config, cols, rows, 0);

    // 10b. Startup banner
    if !config.no_banner {
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

        let mut lines: Vec<String> = image_lines
            .iter()
            .map(|l| format!("{}{}", img_pad, l))
            .collect();
        lines.extend(text_block);

        let _ = terminal::print_to_scroll(&state.stdout, &lines.join("\n"), rows);
    }

    // 11. Fetch available models (5 s timeout; static fallback already in state)
    {
        let models = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            syl_rs::api::models::fetch_models(&http, &config),
        )
        .await
        .unwrap_or_else(|_| {
            tracing::warn!("model list fetch timed out, using static fallback");
            syl_rs::api::models::static_fallback()
        });

        let n = models.len();
        *state.available_models.lock().unwrap() = models;
        let _ = terminal::print_to_scroll(
            &state.stdout,
            &format!("\x1b[2m{n} models available  ·  /model to switch\x1b[0m\r\n"),
            rows,
        );
    }

    // 12. SIGWINCH handler
    let resize_rx = terminal::spawn_sigwinch_handler();

    // 13. SIGINT handler
    let sigint_state = state.clone();
    tokio::spawn(async move { sigint_loop(sigint_state).await });

    // 14. Container stats poller (every 5 s)
    let stats_state = state.clone();
    tokio::spawn(async move { stats_poll_loop(stats_state).await });

    // 15. REPL
    let result = main_loop(
        &http,
        &config,
        state.clone(),
        resize_rx,
        rows,
        cols,
        &syl_dir,
        &session_path,
        &memory_dir,
        &project_dir,
    )
    .await;

    // 16. Teardown: disable raw mode + reset DECSTBM
    crossterm::terminal::disable_raw_mode()?;
    {
        let mut out = state.stdout.lock().unwrap();
        write!(out, "\x1b[r")?; // reset scroll region to full terminal
        let _ = out.flush();
    }

    // 17. Remove the agent container on clean exit
    println!("[Syl] Cleaning up container...");
    container::nuke_container(false);

    result
}

fn print_help() {
    println!(
        "syl — AI coding assistant\n\n\
         Usage: syl [options]\n\n\
         Options:\n  \
         --model <name>      Override model (default: claude-opus-5-5)\n  \
         --backend <name>    anthropic | bedrock  (default: anthropic)\n  \
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

// ---------------------------------------------------------------------------
// Status bars
// ---------------------------------------------------------------------------

fn draw_bars(
    state: &Arc<SharedState>,
    config: &syl_rs::config::AppConfig,
    cols: u16,
    rows: u16,
    message_count: usize,
) {
    let palette = if config.light { &LIGHT } else { &DARK };

    // Read current model from state (may differ from config after /model switch)
    let current_model = state.current_model.lock().unwrap().clone();

    let (input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, cost_usd) = {
        let tok = state.session_tokens.lock().unwrap();
        let cost = syl_rs::pricing::compute_cost_usd(&tok, &current_model);
        (
            tok.input_tokens,
            tok.output_tokens,
            tok.cache_read_tokens,
            tok.cache_write_tokens,
            cost,
        )
    };

    let (cpu_pct, mem_mb) = {
        let stats = state.container_stats.lock().unwrap();
        match stats.as_ref() {
            Some(s) => (s.cpu_percent, s.mem_usage_mb as u64),
            None => (0.0, 0),
        }
    };

    let tokens_per_min = {
        let mut calls = state.recent_calls.lock().unwrap();
        syl_rs::api::rate_limit::prune_old_calls(&mut calls);
        syl_rs::api::rate_limit::window_tokens(&calls) as f64
    };

    let data = BarData {
        model: current_model,
        backend: config.backend.clone(),
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
        message_count,
        width: cols,
        spinner_label: None,
    };

    let top = make_top_bar(&data, palette);
    let bottom = make_bottom_bar(&data, palette);

    let mut out = state.stdout.lock().unwrap();
    let _ = terminal::draw_status_bars(&mut *out, &top, &bottom, rows);
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

async fn sigint_loop(state: Arc<SharedState>) {
    loop {
        tokio::signal::ctrl_c()
            .await
            .expect("ctrl-c handler failed");
        if let Some(token) = state.current_cancel.lock().unwrap().as_ref() {
            token.cancel();
        }
    }
}

async fn stats_poll_loop(state: Arc<SharedState>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if let Some(stats) = container::poll_stats() {
            *state.container_stats.lock().unwrap() = Some(stats);
        }
    }
}

// ---------------------------------------------------------------------------
// REPL
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn main_loop(
    http: &reqwest::Client,
    config: &syl_rs::config::AppConfig,
    state: Arc<SharedState>,
    mut resize_rx: tokio::sync::watch::Receiver<(u16, u16)>,
    mut rows: u16,
    mut cols: u16,
    syl_dir: &Path,
    session_path: &Path,
    memory_dir: &Path,
    project_dir: &Path,
) -> anyhow::Result<()> {
    let mut editor = InputEditor::new()?;
    let workflow_dir = syl_dir.join("workflows");
    let log_path = syl_dir.join("syl.log");

    // Welcome hint
    let _ = terminal::print_to_scroll(
        &state.stdout,
        "Type a message, or /help for commands.\r\n",
        rows,
    );

    loop {
        // Drain any pending resize before showing the prompt
        while resize_rx.has_changed().unwrap_or(false) {
            let (nc, nr) = *resize_rx.borrow_and_update();
            cols = nc;
            rows = nr;
            handle_resize(nc, nr, &state).await?;
        }

        let msg_count = state.messages.lock().unwrap().len();
        draw_bars(&state, config, cols, rows, msg_count);

        let line = match editor.readline("> ", rows) {
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

        // Slash-command dispatch
        if line.starts_with('/') {
            // ── Model selector ────────────────────────────────────────────
            // Handled before the generic dispatcher because it needs direct
            // terminal access for the interactive picker.
            if line == "/model" || line.starts_with("/model ") {
                if let Some(new_model) = run_model_selector(&line, &state, rows).await {
                    *state.current_model.lock().unwrap() = new_model;
                    draw_bars(
                        &state,
                        config,
                        cols,
                        rows,
                        state.messages.lock().unwrap().len(),
                    );
                }
                continue;
            }

            let msgs = state.messages.lock().unwrap().clone();
            let (input_tokens, output_tokens) = {
                let tok = state.session_tokens.lock().unwrap();
                (tok.input_tokens, tok.output_tokens)
            };
            // Read current model from state so /status reflects live selection.
            let current_model = state.current_model.lock().unwrap().clone();
            let slash_ctx = SlashContext {
                session_path,
                workflow_dir: &workflow_dir,
                memory_dir,
                log_path: &log_path,
                model: &current_model,
                backend: &config.backend,
                timeout_ms: config.timeout_ms,
                rate_limit: config.rate_limit,
                input_tokens,
                output_tokens,
                message_count: msgs.len(),
                config,
            };
            if let Some(result) = handle_slash(&line, msgs, &slash_ctx).await {
                *state.messages.lock().unwrap() = result.messages;
                if let Some(text) = result.output {
                    let _ = terminal::print_to_scroll(&state.stdout, &format!("{text}\r\n"), rows);
                }
                if result.action == SlashAction::Exit {
                    break;
                }
                continue;
            }
            let _ = terminal::print_to_scroll(
                &state.stdout,
                &format!("Unknown command: {line}\r\n"),
                rows,
            );
            continue;
        }

        // Push user message
        state
            .messages
            .lock()
            .unwrap()
            .push(Message::user_text(&line));

        // Agent turn
        run_agent_turn(
            http,
            config,
            &state,
            &mut resize_rx,
            &mut rows,
            &mut cols,
            syl_dir,
            memory_dir,
            project_dir,
        )
        .await?;

        // Persist after each turn
        let msgs = state.messages.lock().unwrap().clone();
        if let Err(e) = save_session(session_path, &msgs) {
            tracing::warn!("session save failed: {e}");
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Model selector
// ---------------------------------------------------------------------------

/// Handle the `/model` slash command.
///
/// With no argument: launches the interactive arrow-key picker populated from
/// `state.available_models` (fetched from the API at startup).
/// With an argument: exact or substring match against the live model list;
/// if unrecognised, still switches — the API will validate the ID.
async fn run_model_selector(line: &str, state: &Arc<SharedState>, rows: u16) -> Option<String> {
    let arg = line.strip_prefix("/model").unwrap_or("").trim().to_owned();
    let current = state.current_model.lock().unwrap().clone();

    if arg.is_empty() {
        // Interactive picker — must run synchronously; block_in_place is safe
        // here because we're the only thread touching stdin/stdout.
        let items: Vec<syl_rs::tui::picker::ModelOption> = {
            let models = state.available_models.lock().unwrap();
            models
                .iter()
                .map(|m| {
                    // When display_name differs from id (dynamic API fetch),
                    // show the id dimmed on the right so users can see it.
                    let description = if m.id != m.display_name {
                        m.id.clone()
                    } else {
                        String::new()
                    };
                    syl_rs::tui::picker::ModelOption {
                        id: m.id.clone(),
                        display: m.display_name.clone(),
                        description,
                    }
                })
                .collect()
        };
        let stdout = state.stdout.clone();
        tokio::task::block_in_place(move || {
            syl_rs::tui::picker::run_picker(&items, &current, &stdout, rows)
        })
    } else {
        // Direct switch — match id or display_name (case-insensitive substring).
        // If unrecognised, accept as a raw model ID; the API validates it.
        let canonical: Option<String> = {
            let models = state.available_models.lock().unwrap();
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
        let msg = format!(
            "\x1b[2m[Switched to \x1b[0m\x1b[96m{model_id}\x1b[0m{note}\x1b[2m]\x1b[0m\r\n"
        );
        let _ = terminal::print_to_scroll(&state.stdout, &msg, rows);
        Some(model_id)
    }
}

// ---------------------------------------------------------------------------
// Agent turn
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn run_agent_turn(
    http: &reqwest::Client,
    config: &syl_rs::config::AppConfig,
    state: &Arc<SharedState>,
    resize_rx: &mut tokio::sync::watch::Receiver<(u16, u16)>,
    rows: &mut u16,
    cols: &mut u16,
    syl_dir: &Path,
    memory_dir: &Path,
    project_dir: &Path,
) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    *state.current_cancel.lock().unwrap() = Some(cancel.clone());

    // Build an effective config that reflects the live model selection.
    // Shadow the parameter so all downstream calls use the effective config.
    let effective_config = {
        let model = state.current_model.lock().unwrap().clone();
        let mut c = config.clone();
        if !model.is_empty() {
            c.model = model;
        }
        c
    };
    let config = &effective_config;

    // Start thinking animation
    {
        let anim = animation::spawn_animation(state.stdout.clone(), *rows, *cols);
        *state.anim.lock().unwrap() = Some(anim);
    }

    loop {
        // The system prompt (persona + workspace snapshot + memories) is built
        // once per conversation and then frozen: editing `system` mid-
        // conversation fails the thinking-block prefix check on Opus 5.5 and
        // other models with preserved thinking.  It is rebuilt only while no
        // assistant turn exists yet (new session or after /clear).
        let system: Vec<SystemBlock> = {
            let fresh_conversation = !state
                .messages
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.role == "assistant");
            let mut cached = state.system_prompt.lock().unwrap();
            if fresh_conversation || cached.is_none() {
                let workspace = gather_workspace_context(project_dir);
                let blocks: Vec<SystemBlock> = build_system_blocks(&workspace, memory_dir)
                    .into_iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text {
                            text,
                            cache_control,
                        } => Some(if cache_control.is_some() {
                            SystemBlock::cached(text)
                        } else {
                            SystemBlock::text(text)
                        }),
                        _ => None,
                    })
                    .collect();
                *cached = Some(blocks);
            }
            cached.clone().unwrap_or_default()
        };

        // Compact before every model call, not just between user messages:
        // a long tool chain inside one turn can otherwise grow without bound.
        // (No-op until history exceeds MAX_HISTORY.)
        {
            let msgs = state.messages.lock().unwrap().clone();
            if msgs.len() > syl_rs::api::compaction::MAX_HISTORY {
                let compacted = syl_rs::api::compaction::compact_history(
                    msgs,
                    http,
                    config,
                    state,
                    cancel.clone(),
                )
                .await;
                *state.messages.lock().unwrap() = compacted;
            }
        }

        let messages = state.messages.lock().unwrap().clone();

        // Rows are read live by the callbacks so a resize mid-stream is honoured
        // without restarting the request.
        let live_rows = Arc::new(AtomicU16::new(*rows));

        let notify_fn = {
            let st = state.clone();
            let lr = live_rows.clone();
            move |msg: &str| {
                let r = lr.load(Ordering::Relaxed);
                let _ = terminal::print_to_scroll(&st.stdout, &format!("{msg}\r\n"), r);
            }
        };

        // Per-iteration flag: did any text deltas arrive this turn?
        let streaming_started = Arc::new(AtomicBool::new(false));

        let on_delta = {
            let st = state.clone();
            let lr = live_rows.clone();
            let ss = streaming_started.clone();
            move |delta: BlockDeltaEvent| {
                handle_stream_delta(&st, delta, lr.load(Ordering::Relaxed), &ss)
            }
        };

        // Keep the same in-flight request alive across terminal resizes.
        // (Previously a resize dropped the stream and re-sent the whole call,
        // which double-billed and printed partly duplicated text.)
        let call = call_with_retry_shared(
            http, config, state, &messages, &system, cancel.clone(), notify_fn, on_delta,
        );
        tokio::pin!(call);
        let mut resize_open = true;
        let result = loop {
            tokio::select! {
                r = &mut call => break r,
                changed = resize_rx.changed(), if resize_open => {
                    if changed.is_err() {
                        // Resize sender gone; just wait for the call.
                        resize_open = false;
                        continue;
                    }
                    let (nc, nr) = *resize_rx.borrow_and_update();
                    *cols = nc;
                    *rows = nr;
                    live_rows.store(nr, Ordering::Relaxed);
                    let _ = handle_resize(nc, nr, state).await;
                    if streaming_started.load(Ordering::Relaxed) {
                        // Text is already flowing: no thinking animation, and
                        // re-anchor the streaming line inside the new region.
                        if let Some(anim) = state.anim.lock().unwrap().take() {
                            anim.stop();
                        }
                        let _ = terminal::end_streaming(&state.stdout, nr);
                        let _ = terminal::begin_streaming(&state.stdout, nr);
                    }
                }
            }
        };

        // Close streaming line before processing result
        if streaming_started.load(Ordering::Relaxed) {
            let _ = terminal::end_streaming(&state.stdout, *rows);
        }

        let turn = match result {
            Ok(t) => t,
            Err(e) if e.downcast_ref::<CancelledError>().is_some() => {
                tracing::info!("turn cancelled by user");
                let _ = terminal::print_to_scroll(&state.stdout, "[cancelled]\r\n", *rows);
                break;
            }
            Err(e) => {
                let _ =
                    terminal::print_to_scroll(&state.stdout, &format!("[Error: {e}]\r\n"), *rows);
                break;
            }
        };

        // Stop animation now that streaming finished
        if let Some(anim) = state.anim.lock().unwrap().take() {
            anim.stop();
        }

        // Accumulate token usage
        {
            let mut tok = state.session_tokens.lock().unwrap();
            tok.input_tokens += turn.usage.input_tokens;
            tok.output_tokens += turn.usage.output_tokens;
            tok.cache_read_tokens += turn.usage.cache_read_tokens;
            tok.cache_write_tokens += turn.usage.cache_write_tokens;
        }

        // Append assistant turn to history
        state
            .messages
            .lock()
            .unwrap()
            .push(Message::assistant_blocks(turn.content.clone()));

        if turn.stop_reason != StopReason::ToolUse {
            break;
        }

        // Collect ToolUse blocks
        let tool_calls: Vec<(String, String, serde_json::Value)> = turn
            .content
            .into_iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => Some((id, name, input)),
                _ => None,
            })
            .collect();

        if tool_calls.is_empty() {
            break;
        }

        // Swap animation for spinner
        if let Some(anim) = state.anim.lock().unwrap().take() {
            anim.stop();
        }
        let sp = spinner::spawn_spinner(
            state.stdout.clone(),
            *rows,
            format!("{} tool(s)…", tool_calls.len()),
        );
        *state.spinner.lock().unwrap() = Some(sp);

        // Execute tools
        let workflow_dir = syl_dir.join("workflows");
        let results = execute_tools(
            tool_calls,
            config,
            state,
            http,
            cancel.clone(),
            *rows,
            *cols,
            syl_dir,
            memory_dir,
            &workflow_dir,
        )
        .await;

        // Stop spinner
        if let Some(sp) = state.spinner.lock().unwrap().take() {
            sp.stop();
        }

        // Append tool results and restart animation for next LLM call
        state.messages.lock().unwrap().push(Message {
            role: "user".to_string(),
            content: MessageContent::Blocks(results),
        });

        let anim = animation::spawn_animation(state.stdout.clone(), *rows, *cols);
        *state.anim.lock().unwrap() = Some(anim);
    }

    // Cleanup
    if let Some(anim) = state.anim.lock().unwrap().take() {
        anim.stop();
    }
    if let Some(sp) = state.spinner.lock().unwrap().take() {
        sp.stop();
    }
    *state.current_cancel.lock().unwrap() = None;

    Ok(())
}

// ---------------------------------------------------------------------------
// Stream delta callback
// ---------------------------------------------------------------------------

fn handle_stream_delta(
    state: &Arc<SharedState>,
    delta: BlockDeltaEvent,
    rows: u16,
    started: &AtomicBool,
) {
    match delta {
        BlockDeltaEvent::ThinkingDelta { text } | BlockDeltaEvent::TextDelta { text } => {
            if !started.swap(true, Ordering::Relaxed) {
                // First text delta: stop animation and open a new scroll line
                if let Some(anim) = state.anim.lock().unwrap().take() {
                    anim.stop();
                }
                let _ = terminal::begin_streaming(&state.stdout, rows);
            }
            let _ = terminal::write_streaming(&state.stdout, &text);
        }
        BlockDeltaEvent::InputJsonDelta { .. } => {
            // Partial tool input — accumulates in SseParser, don't display
        }
    }
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

async fn execute_tools(
    calls: Vec<(String, String, serde_json::Value)>,
    config: &syl_rs::config::AppConfig,
    state: &Arc<SharedState>,
    client: &reqwest::Client,
    cancel: CancellationToken,
    rows: u16,
    cols: u16,
    syl_dir: &Path,
    memory_dir: &Path,
    workflow_dir: &Path,
) -> Vec<ContentBlock> {
    // Health-check container before running any tools
    if let Err(e) = syl_rs::container::ensure_container(config, syl_dir) {
        let _ = terminal::print_to_scroll(
            &state.stdout,
            &format!("[Syl] Container recovery failed: {e}\r\n"),
            rows,
        );
    }

    let mut results = Vec::new();

    for (id, name, input) in calls {
        // Update spinner label for current tool
        {
            if let Some(sp) = state.spinner.lock().unwrap().take() {
                sp.stop();
            }
            let sp = spinner::spawn_spinner(state.stdout.clone(), rows, format!("{name}…"));
            *state.spinner.lock().unwrap() = Some(sp);
        }

        let (content, is_error) = match name.as_str() {
            "run_bash" => {
                tools::bash::run_bash(&input, config, Default::default(), cancel.clone()).await
            }
            "memory_read" => (
                syl_rs::tools::memory::memory_read(memory_dir, input["key"].as_str().unwrap_or("")),
                false,
            ),
            "memory_write" => {
                let key = input["key"].as_str().unwrap_or("").to_string();
                let content = input["content"].as_str().unwrap_or("").to_string();
                (
                    syl_rs::tools::memory::memory_write(memory_dir, &key, &content),
                    false,
                )
            }
            "workflow_write" => {
                let key = input["name"].as_str().unwrap_or("").to_string();
                let content = input["content"].as_str().unwrap_or("").to_string();
                (
                    syl_rs::tools::workflow::workflow_write(workflow_dir, &key, &content),
                    false,
                )
            }
            "workflow_delete" => {
                let key = input["name"].as_str().unwrap_or("").to_string();
                (
                    syl_rs::tools::workflow::workflow_delete(workflow_dir, &key),
                    false,
                )
            }
            "read_file" => tools::files::read_file(&input, config),
            "dispatch_agent" => {
                tools::dispatch::dispatch_agent(&input, config, state, client, cancel.clone()).await
            }
            "show_image" => syl_rs::tools::image::show_image(&input, config, state, rows, cols),
            other => (format!("[Tool error: unknown tool `{other}`]"), true),
        };

        // Per-tool TUI display
        let display = match name.as_str() {
            "run_bash" => {
                // Bordered output clipped to 40 lines
                let lines: Vec<&str> = content.lines().collect();
                let clip: Vec<&str> = lines.iter().take(40).copied().collect();
                let omitted = lines.len().saturating_sub(40);
                let mut out = format!("┌─ bash ─────────────────────────────────\r\n");
                for l in &clip {
                    out.push_str(&format!("│ {l}\r\n"));
                }
                if omitted > 0 {
                    out.push_str(&format!("│ ... ({omitted} more lines)\r\n"));
                }
                out.push_str("└────────────────────────────────────────");
                out
            }
            "memory_read" | "memory_write" => {
                format!("[memory] {content}")
            }
            "workflow_write" | "workflow_delete" => {
                format!("[workflow] {content}")
            }
            "read_file" => {
                let path = input["path"].as_str().unwrap_or("?");
                format!("[read_file: {path}] ({} chars)", content.len())
            }
            "dispatch_agent" => {
                let source = input["source"].as_str().unwrap_or("?");
                let task = input["task"].as_str().unwrap_or("?");
                format!(
                    "[dispatch_agent: {task} {source}] ({} chars)",
                    content.len()
                )
            }
            "show_image" => {
                let path = input["path"].as_str().unwrap_or("?");
                format!("[show_image: {path}]")
            }
            _ => content.clone(),
        };
        let _ = terminal::print_to_scroll(&state.stdout, &format!("{display}\r\n"), rows);

        results.push(ContentBlock::ToolResult {
            tool_use_id: id,
            content,
            is_error: if is_error { Some(true) } else { None },
        });
    }

    results
}

// ---------------------------------------------------------------------------
// Resize handling
// ---------------------------------------------------------------------------

async fn handle_resize(
    new_cols: u16,
    new_rows: u16,
    state: &Arc<SharedState>,
) -> anyhow::Result<()> {
    // Stop stale animation/spinner (they hold old row numbers)
    if let Some(anim) = state.anim.lock().unwrap().take() {
        anim.stop();
    }
    if let Some(sp) = state.spinner.lock().unwrap().take() {
        sp.stop();
    }

    // Re-establish DECSTBM with new dimensions
    {
        let mut out = state.stdout.lock().unwrap();
        terminal::setup_scroll_region(&mut *out, new_rows)?;
    }

    // Restart animation if a turn is active
    if state.current_cancel.lock().unwrap().is_some() {
        let anim = animation::spawn_animation(state.stdout.clone(), new_rows, new_cols);
        *state.anim.lock().unwrap() = Some(anim);
    }

    Ok(())
}
