//! Syl without the terminal UI: `syl -p` (one turn, for scripts and CI)
//! and `syl acp` (an Agent Client Protocol agent on stdio, for editors such
//! as Zed and for Bondsmith).
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use syl_container::{Container, ContainerOptions};
use syl_core::agent::{self, Agent, AgentCommand, AgentEvent, Mode, SessionPaths, SessionStart, TurnEnd};
use syl_core::config::{build_config, AppConfig, CliArgs};
use syl_core::control::PermissionAnswer;
use syl_core::model::AnthropicAdapter;
use syl_core::session::find_session;
use syl_core::shared::SharedState;

/// The container a project's config asks for.
pub fn container_for(project_dir: &Path, config: &AppConfig) -> Container {
    let defaults = ContainerOptions::default();
    Container::for_project(project_dir).with_options(ContainerOptions {
        network: config.container.network.clone(),
        memory: config.container.memory.clone().unwrap_or(defaults.memory),
        cpus: config.container.cpus.clone(),
    })
}

fn http_client(config: &AppConfig) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder().timeout(std::time::Duration::from_millis(config.timeout_ms)).build()?)
}

/// `syl -p "prompt"`: run one turn and print the answer.
///
/// - The answer's text streams to stdout; notices and errors go to stderr.
/// - With `--output json`, every agent event is a JSON line on stdout.
/// - Nobody can answer a permission question, so `ask` rules refuse.
/// - Exit status: 0 when the turn completes, 2 when the answer was cut off
///   (max_tokens) or refused, 1 when it fails (or a `--resume` can't be
///   resumed), 130 when it's cancelled (Ctrl+C, or stdout closed early).
/// - The container is left running for the next run.
pub async fn run_print(cli: &CliArgs) -> anyhow::Result<i32> {
    use std::io::IsTerminal;
    let mut prompt = cli.print.clone().unwrap_or_default();
    if prompt.trim().is_empty() {
        if std::io::stdin().is_terminal() {
            anyhow::bail!("-p needs a prompt: give it as an argument, or pipe it in");
        }
        std::io::stdin().read_to_string(&mut prompt)?;
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("-p needs a prompt (as an argument, or on stdin)");
    }

    let project_dir = std::env::current_dir()?;
    let syl_dir = project_dir.join(".syl");
    std::fs::create_dir_all(&syl_dir)?;
    let _log_guard = syl_core::logging::init_logging(&syl_dir)?;
    let config = build_config(cli, &syl_dir);
    if let Some(e) = &config.config_error {
        eprintln!("[syl] config.toml can't be read, so every tool call asks (and -p refuses it): {e}");
    }
    for w in &config.config_warnings {
        eprintln!("[syl] {w}");
    }
    if let Some(n) = config.untrusted_notice() {
        eprintln!("[syl] {n}");
    }

    let paths = SessionPaths::for_project(&project_dir);
    let start = match &cli.resume {
        Some(query) => {
            let dir = paths.sessions_dir.clone().expect("standard layout saves sessions");
            match find_session(&dir, query) {
                Ok(id) => SessionStart::Resume(id),
                Err(e) => anyhow::bail!("--resume: {e}"),
            }
        }
        None => SessionStart::New,
    };

    let container = Arc::new(container_for(&project_dir, &config));
    {
        let c = container.clone();
        let fresh = cli.fresh;
        tokio::task::spawn_blocking(move || c.start_or_resume(fresh, &|m| eprintln!("{m}"))).await??;
    }

    let http = http_client(&config)?;
    let state = SharedState::new();
    *state.current_model.lock().unwrap() = config.model.clone();
    let model = Arc::new(AnthropicAdapter::new(http, state.clone()));
    let json = cli.output_json;
    let mut handle = agent::spawn(Agent { config, state, model, container, paths, start });

    {
        let commands = handle.commands.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                let _ = commands.send(AgentCommand::Cancel);
            }
        });
    }
    // The session first: a --resume that can't resume is an error here, not
    // a quiet new session.
    let resuming = cli.resume.is_some();
    loop {
        match handle.events.recv().await {
            Some(AgentEvent::SessionChanged { info }) => {
                if json {
                    println!("{}", serde_json::to_string(&AgentEvent::SessionChanged { info }).unwrap_or_default());
                }
                break;
            }
            Some(AgentEvent::Error { message }) if resuming => {
                eprintln!("[syl] {message}");
                let _ = handle.commands.send(AgentCommand::Shutdown);
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle.task).await;
                return Ok(1);
            }
            Some(_) => {}
            None => anyhow::bail!("the agent stopped"),
        }
    }
    if cli.plan {
        let _ = handle.commands.send(AgentCommand::SetMode(Mode::Plan));
    }
    let attached = syl_core::attachments::attach_images(&prompt, &project_dir);
    for p in &attached.problems {
        eprintln!("[syl] image not attached: {p}");
    }
    let cmd = if attached.images.is_empty() {
        AgentCommand::Prompt(prompt)
    } else {
        AgentCommand::PromptWithImages { text: attached.text, images: attached.images }
    };
    let _ = handle.commands.send(cmd);

    let mut out = std::io::stdout().lock();
    let mut ended_with_text = true;
    let mut wrote_text = false;
    let mut turn_started = false;
    let mut stdout_gone = false;
    let code = loop {
        let Some(ev) = handle.events.recv().await else { break 1 };
        // If whoever reads our stdout went away (`| head`), stop the turn
        // rather than keep running tools and spending tokens.
        let mut write = |s: &str| {
            if !stdout_gone && (out.write_all(s.as_bytes()).is_err() || out.flush().is_err()) {
                stdout_gone = true;
                let _ = handle.commands.send(AgentCommand::Cancel);
            }
        };
        if json {
            write(&format!("{}\n", serde_json::to_string(&ev).unwrap_or_default()));
        }
        match &ev {
            AgentEvent::TurnStarted => turn_started = true,
            // Separate model replies in one turn: keep them apart.
            AgentEvent::RequestStarted if !json && wrote_text => {
                write(if ended_with_text { "\n" } else { "\n\n" });
                ended_with_text = true;
                wrote_text = false;
            }
            AgentEvent::TextDelta { text } if !json => {
                write(text);
                ended_with_text = text.ends_with('\n');
                wrote_text = true;
            }
            AgentEvent::Notice { message } if !json => eprintln!("{message}"),
            AgentEvent::Error { message } if !json => eprintln!("[syl] error: {message}"),
            AgentEvent::PermissionRequested { id, name, subject, .. } => {
                eprintln!("[syl] refused {name}: `{subject}` needs approval, and -p can't ask");
                let _ = handle.commands.send(AgentCommand::ApprovePermission { id: id.clone(), answer: PermissionAnswer::Reject });
            }
            AgentEvent::TurnEnded { reason } if turn_started => {
                use syl_core::api::types::StopReason;
                break match reason {
                    TurnEnd::Completed(StopReason::MaxTokens | StopReason::ContextWindowExceeded) => {
                        eprintln!("[syl] the answer was cut off (max_tokens)");
                        2
                    }
                    TurnEnd::Completed(StopReason::Other(r)) if r == "refusal" => {
                        eprintln!("[syl] the model declined to answer");
                        2
                    }
                    TurnEnd::Completed(_) => 0,
                    TurnEnd::Cancelled => 130,
                    TurnEnd::Failed => 1,
                };
            }
            _ => {}
        }
    };
    if !json && !ended_with_text {
        let _ = writeln!(out);
    }
    drop(out);
    let _ = handle.commands.send(AgentCommand::Shutdown);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle.task).await;
    Ok(code)
}

/// `syl acp`: serve the Agent Client Protocol on stdin/stdout. Logs go to
/// `~/.syl/acp.log` (stdout carries protocol messages only).
pub async fn run_acp() -> anyhow::Result<()> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let log_dir = home.join(".syl");
    let _log_guard = syl_core::logging::init_logging_named(&log_dir, "acp.log")?;
    let base = AppConfig::default();
    let opts = syl_core::acp::AcpOptions {
        container: Arc::new(|dir: &Path, config: &AppConfig| container_for(dir, config)),
        config: Arc::new(|dir: &Path| {
            let syl_dir = dir.join(".syl");
            let _ = std::fs::create_dir_all(&syl_dir);
            build_config(&CliArgs::default(), &syl_dir)
        }),
        http: http_client(&base)?,
        start_containers: true,
    };
    syl_core::acp::serve(tokio::io::stdin(), tokio::io::stdout(), opts).await
}
