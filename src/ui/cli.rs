//! CLI configuration and terminal conversation controller.
use super::{StreamDisplay, TerminalFrontend, input, live, styles_enabled};
use crate::{
    INTERRUPTED, Workspace, check_interrupt, compaction,
    engine::text,
    runner::{TurnModel, connect, turn as turn_compacting},
    session::{self, Session, Store},
    skills,
};
#[cfg(test)]
use llm_api::Request;
use llm_api::{Cancelled, Client, Provider, Result};
use serde_json::json;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::Ordering;

pub(crate) fn run() -> Result<()> {
    let mut provider = std::env::var("AGENT_PROVIDER").unwrap_or("codex".into());
    let mut model = std::env::var("AGENT_MODEL").unwrap_or(crate::models::DEFAULT_MODEL.into());
    let mut yes = true;
    let mut verbose = false;
    let mut max_turns = 500;
    let mut max_context_kib: usize = 4096;
    let mut resume = None;
    let mut prompt = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--list-skills" => {
                let workspace = Workspace {
                    root: std::env::current_dir()?.canonicalize()?,
                    yes: false,
                    frontend: Box::new(TerminalFrontend { verbose: false }),
                };
                println!("{}", skills::catalog(&skills::discover(&workspace)));
                return Ok(());
            }
            "--resume" => resume = Some(args.next().ok_or("--resume needs a session ID")?),
            "--verbose" => verbose = true,
            "--yes" => yes = true,
            "--ask" => yes = false,
            "--provider" => provider = args.next().ok_or("--provider needs a value")?,
            "--model" => model = args.next().ok_or("--model needs a value")?,
            "--max-turns" => max_turns = args.next().ok_or("--max-turns needs a value")?.parse()?,
            "--max-context-kib" => {
                max_context_kib = args
                    .next()
                    .ok_or("--max-context-kib needs a value")?
                    .parse()?
            }
            "--help" | "-h" => {
                println!(
                    "/compact summarizes older conversation. Automatic compaction defaults to 128 KiB of message content; set AGENT_COMPACT_KIB=0 to disable it."
                );
                println!(
                    "Interactive terminal: Enter queues a visible follow-up; Tab steers at the next model/tool boundary. ↑ from empty input selects pending messages (Tab: steer, e: edit, Delete: remove). Esc interrupts and preserves pending input for selection and sending; Ctrl-C cancels and discards it."
                );
                println!(
                    "ag [--provider openai|anthropic|openrouter|codex] [--model MODEL] [--ask|--yes] [--verbose] [--list-skills] [--resume ID] [--max-turns N] [--max-context-kib N] [--] [PROMPT...]\nDefaults: provider codex, model gpt-6.2-sol (overridable with AGENT_PROVIDER/AGENT_MODEL).\nLimits: 500 model calls per prompt, 4096 KiB serialized context (not a token limit; provider limits still apply).\nNo prompt: interactive conversation. /quit exits; /clear starts a new saved session.\n/model opens a searchable model picker; /model ID selects a custom model for the current provider. Switching starts a new saved session; availability depends on your account.\n/resume opens a searchable session picker (↑/↓, Enter, Esc); /resume ID restores one without replaying tools.\n/reload saves and restarts the executable at its current path, preserving context. Sessions are private files under $AGENT_HOME/sessions (default ~/.agent/sessions); they may contain sensitive tool output.\nTool calls are allowed without confirmation by default, including unsandboxed shell commands with full user privileges.\n--ask requires confirmation for shell commands and file writes; --yes restores automatic approval.\nTool activity is compact by default; --verbose shows full arguments and bounded results."
                );
                return Ok(());
            }
            "--" => {
                prompt.extend(args);
                break;
            }
            _ if arg.starts_with('-') => return Err(format!("unknown option {arg}").into()),
            _ => {
                prompt.push(arg);
                prompt.extend(args);
                break;
            }
        }
    }
    if model.is_empty() || max_turns == 0 {
        return Err("set --model/AGENT_MODEL and a positive --max-turns".into());
    }
    let max_context_bytes = max_context_kib
        .checked_mul(1024)
        .filter(|bytes| *bytes > 0)
        .ok_or("--max-context-kib must be positive and fit in bytes")?;
    let provider: Provider = serde_json::from_value(json!(provider))?;
    let workspace = Workspace {
        root: std::env::current_dir()?.canonicalize()?,
        yes,
        frontend: Box::new(TerminalFrontend { verbose }),
    };
    let root = session::directory()?;
    if let Some(id) = resume {
        let (session, provider) = Store::load(&root, &workspace.root, &id)?;
        return chat_session(
            connect(provider)?,
            workspace,
            session,
            provider,
            prompt,
            max_turns,
            max_context_bytes,
            verbose,
        );
    }
    let client = connect(provider)?;
    let session = crate::runner::create(&workspace, provider, model)?;
    chat_session(
        client,
        workspace,
        session,
        provider,
        prompt,
        max_turns,
        max_context_bytes,
        verbose,
    )
}

#[cfg(test)]
pub(crate) fn chat(
    client: Client,
    workspace: Workspace,
    request: Request,
    prompt: Vec<String>,
    max_turns: usize,
    max_context_bytes: usize,
) -> Result<()> {
    chat_session(
        client,
        workspace,
        Session {
            request,
            store: None,
        },
        Provider::Openai,
        prompt,
        max_turns,
        max_context_bytes,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn chat_session(
    mut client: Client,
    workspace: Workspace,
    mut session: Session,
    mut provider: Provider,
    prompt: Vec<String>,
    max_turns: usize,
    max_context_bytes: usize,
    verbose: bool,
) -> Result<()> {
    let compact_threshold = compaction::threshold()?;
    // Capture the path before a rebuild replaces the running executable's inode.
    let executable = std::env::current_exe()?;
    let interactive = prompt.is_empty();
    if interactive && !io::stdin().is_terminal() {
        return Err("provide a prompt argument for noninteractive use".into());
    }
    // Keep noncanonical, no-echo input active during model/tool output too.
    // Otherwise the terminal line discipline edits or discards typeahead before
    // the prompt editor can see it. No reader competes with the prompt editor.
    if interactive && provider == Provider::Codex {
        crate::models::start();
    }
    let mut terminal = if interactive {
        Some(input::Terminal::enter()?)
    } else {
        None
    };
    let mut typing = live::Typing::default();
    if session.store.is_some() {
        preview(&session, verbose);
    }
    loop {
        let input = if interactive {
            eprintln!();
            let input = match typing.read_line() {
                Ok(input) => input,
                Err(e) if e.is::<llm_api::Cancelled>() => {
                    INTERRUPTED.store(false, Ordering::Relaxed);
                    eprintln!("cancelled");
                    continue;
                }
                Err(e) => return Err(e),
            };
            if input.is_empty() {
                break;
            }
            match input.trim() {
                "/compact" => {
                    let busy = typing.busy()?;
                    let result = compaction::manual(
                        &mut session.request,
                        &client,
                        session.store.as_ref(),
                        compaction_notice,
                    );
                    if let Some(busy) = busy {
                        busy.finish(&mut typing)?;
                    }
                    if result.as_ref().is_err_and(|error| error.is::<Cancelled>()) {
                        INTERRUPTED.store(false, Ordering::Relaxed);
                        eprintln!("compaction cancelled");
                    } else {
                        result?;
                    }
                    continue;
                }
                "/quit" => break,
                "/clear" => {
                    session.request.messages.clear();
                    if session.store.is_some() {
                        session.store = Some(Store::create(
                            &session::directory()?,
                            &workspace.root,
                            provider,
                            &session.request,
                        )?);
                    }
                    let mut terminal = io::stderr().lock();
                    if terminal.is_terminal() {
                        // Home the cursor, erase the screen, and erase scrollback.
                        terminal.write_all(b"\x1b[H\x1b[2J\x1b[3J")?;
                    }
                    continue;
                }
                command if command == "/model" || command.starts_with("/model ") => {
                    let selected = if command == "/model" {
                        super::picker::choose_model(provider, &session.request.model)
                    } else {
                        Ok(Some(
                            command.trim_start_matches("/model ").trim().to_owned(),
                        ))
                    };
                    match selected.and_then(|model| {
                        model
                            .map(|model| session.switch_model(&model, provider))
                            .transpose()
                    }) {
                        Ok(Some(changed)) => {
                            eprintln!(
                                "Model: {}{}",
                                super::display(&session.request.model),
                                if changed {
                                    " (new session; previous conversation saved)"
                                } else {
                                    " (unchanged)"
                                }
                            );
                        }
                        Ok(None) => {}
                        Err(e) if e.is::<llm_api::Cancelled>() => {
                            INTERRUPTED.store(false, Ordering::Relaxed);
                        }
                        Err(error) => eprintln!(
                            "model selection failed: {}",
                            super::display(&error.to_string())
                        ),
                    }
                    continue;
                }
                "/reload" => {
                    session.save(false)?;
                    if let Some(store) = &session.store {
                        use std::os::unix::process::CommandExt;
                        // exec does not run destructors: restore the original
                        // terminal mode before replacing this process.
                        drop(terminal.take());
                        let error = std::process::Command::new(&executable)
                            .current_dir(&workspace.root)
                            .args([
                                "--resume",
                                &store.id,
                                if workspace.yes { "--yes" } else { "--ask" },
                            ])
                            .args(if verbose { vec!["--verbose"] } else { vec![] })
                            .args([
                                "--max-turns",
                                &max_turns.to_string(),
                                "--max-context-kib",
                                &(max_context_bytes / 1024).to_string(),
                            ])
                            .exec();
                        terminal = Some(input::Terminal::enter()?);
                        eprintln!("reload failed: {error}");
                    }
                    continue;
                }
                command if command == "/resume" || command.starts_with("/resume ") => {
                    let root = session::directory()?;
                    let current = session.store.as_ref().map(|store| store.id.as_str());
                    let selected = if command == "/resume" {
                        Store::list(&root, &workspace.root)
                            .and_then(|entries| super::picker::choose(&entries, current))
                    } else {
                        Ok(Some(
                            command.trim_start_matches("/resume ").trim().to_owned(),
                        ))
                    };
                    let id = match selected {
                        Ok(Some(id)) => id,
                        Ok(None) => continue,
                        Err(e) if e.is::<llm_api::Cancelled>() => {
                            INTERRUPTED.store(false, Ordering::Relaxed);
                            continue;
                        }
                        Err(error) => {
                            eprintln!("resume failed: {}", super::display(&error.to_string()));
                            continue;
                        }
                    };
                    if current == Some(id.as_str()) {
                        preview(&session, verbose);
                        continue;
                    }
                    match Store::load(&root, &workspace.root, &id)
                        .and_then(|(saved, provider)| Ok((saved, provider, connect(provider)?)))
                    {
                        Ok((saved, saved_provider, saved_client)) => {
                            session = saved;
                            provider = saved_provider;
                            client = saved_client;
                            preview(&session, verbose);
                        }
                        Err(error) => {
                            eprintln!("resume failed: {}", super::display(&error.to_string()))
                        }
                    }
                    continue;
                }
                "" => continue,
                _ => input,
            }
        } else {
            prompt.join(" ")
        };
        session.request.messages.push(text("user", input));
        session.save(true)?;
        let store = &session.store;
        let mut checkpoint_failed = false;
        let busy = if interactive { typing.busy()? } else { None };
        let result = turn_compacting(
            &mut session.request,
            &workspace,
            max_turns,
            max_context_bytes,
            TurnModel::new(&mut client, store.as_ref()),
            compaction::automatic(store.as_ref(), compact_threshold, compaction_notice),
            |client, req| {
                let styled = styles_enabled(io::stdout().is_terminal());
                let mut stream = StreamDisplay::new(styled);
                let mut displayed = false;
                let result = client.run_cancellable(req, &INTERRUPTED, |event| {
                    if check_interrupt().is_err() {
                        return false;
                    }
                    let chunk = stream.delta(&event);
                    if !chunk.is_empty() {
                        displayed = true;
                        return live::output(&chunk, false).is_ok();
                    }
                    true
                });
                if result
                    .as_ref()
                    .is_err_and(|error| error.is::<llm_api::CodexQuota>())
                {
                    // Already printed deltas cannot be retracted. Do not flush any
                    // buffered text, and mark the visible part as discarded.
                    if displayed {
                        live::output("\n[Partial model output discarded]\n", false)?;
                    }
                } else {
                    live::output(&stream.finish(), false)?;
                }
                result
            },
            |request| {
                if let Some(store) = store {
                    if let Err(error) = store.save(request, true) {
                        checkpoint_failed = true;
                        return Err(error);
                    }
                }
                Ok(())
            },
        );
        if let Some(busy) = busy {
            busy.finish(&mut typing)?;
        }
        if checkpoint_failed {
            return result;
        }
        if interactive && INTERRUPTED.swap(false, Ordering::Relaxed) {
            session.request.messages.push(text("user", "Previous turn cancelled. Completed tool effects may remain; inspect state before retrying anything.".into()));
            session.save(false)?;
            if typing.interrupted() {
                eprintln!(
                    "interrupted; completed changes were not rolled back; pending input preserved — select a message or type a continuation"
                );
            } else {
                eprintln!("cancelled; completed changes were not rolled back");
            }
            continue;
        }
        result?;
        session.save(false)?;
        if !interactive {
            break;
        }
    }
    Ok(())
}

fn compaction_notice(message: &str) {
    let _ = live::output(&format!("{}\n", super::display(message)), true);
}

fn preview(session: &Session, verbose: bool) {
    if let Some(store) = &session.store {
        eprintln!("Session: {}", store.id);
    }
    eprintln!("Model: {}", super::display(&session.request.model));
    eprint!(
        "{}",
        super::render_history(
            &session.request.messages,
            styles_enabled(io::stderr().is_terminal()),
            verbose,
        )
    );
}
