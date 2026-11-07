//! CLI configuration and terminal conversation controller.
use super::{StreamDisplay, TerminalFrontend, input, styles_enabled};
use crate::{
    INTERRUPTED, Workspace, auth, check_interrupt,
    engine::{text, tools, turn_saved},
    session::{self, Session, Store},
    skills,
};
use llm_api::{Client, Provider, Request, Result};
use serde_json::json;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::Ordering;

pub(crate) fn run() -> Result<()> {
    let mut provider = std::env::var("AGENT_PROVIDER").unwrap_or("codex".into());
    let mut model = std::env::var("AGENT_MODEL").unwrap_or("gpt-6.1-sol".into());
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
                    "agent [--provider openai|anthropic|openrouter|codex] [--model MODEL] [--ask|--yes] [--verbose] [--list-skills] [--resume ID] [--max-turns N] [--max-context-kib N] [--] [PROMPT...]\nDefaults: provider codex, model gpt-6.1-sol (overridable with AGENT_PROVIDER/AGENT_MODEL).\nLimits: 500 model calls per prompt, 4096 KiB serialized context (not a token limit; provider limits still apply).\nNo prompt: interactive conversation. /quit exits; /clear starts a new saved session.\n/resume opens a searchable session picker (↑/↓, Enter, Esc); /resume ID restores one without replaying tools.\n/reload saves and restarts the executable at its current path, preserving context. Sessions are private files under $AGENT_HOME/sessions (default ~/.agent/sessions); they may contain sensitive tool output.\nTool calls are allowed without confirmation by default, including unsandboxed shell commands with full user privileges.\n--ask requires confirmation for shell commands and file writes; --yes restores automatic approval.\nTool activity is compact by default; --verbose shows full arguments and bounded results."
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
    let mut request = Request {
        model,
        messages: Vec::new(),
        system: format!(
            "{}\nWorkspace: {}",
            include_str!("../prompt.txt"),
            workspace.root.display()
        ),
        tools: tools(),
        json_schema: None,
        max_tokens: 8192,
        options: if provider == Provider::Codex {
            // Codex otherwise may return only opaque replay, with no visible summary.
            serde_json::from_value(json!({"reasoning":{"summary":"auto"}}))?
        } else {
            Default::default()
        },
    };
    request
        .system
        .push_str(&skills::catalog(&skills::discover(&workspace)));
    let instructions = workspace.root.join("AGENTS.md");
    if instructions.exists() {
        request.system.push_str(&format!(
            "\nRoot AGENTS.md:\n{}",
            workspace.read(&workspace.path("AGENTS.md")?)?
        ));
    }
    let client = connect(provider)?;
    let store = Store::create(&root, &workspace.root, provider, &request)?;
    chat_session(
        client,
        workspace,
        Session {
            request,
            store: Some(store),
        },
        provider,
        prompt,
        max_turns,
        max_context_bytes,
        verbose,
    )
}

fn connect(provider: Provider) -> Result<Client> {
    if provider == Provider::Codex {
        return auth::codex_client();
    }
    let key = match provider {
        Provider::Anthropic => "ANTHROPIC_API_KEY",
        Provider::Openrouter => "OPENROUTER_API_KEY",
        _ => "OPENAI_API_KEY",
    };
    Ok(Client::new(provider, std::env::var(key)?))
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
    // Capture the path before a rebuild replaces the running executable's inode.
    let executable = std::env::current_exe()?;
    let interactive = prompt.is_empty();
    if interactive && !io::stdin().is_terminal() {
        return Err("provide a prompt argument for noninteractive use".into());
    }
    // Keep noncanonical, no-echo input active during model/tool output too.
    // Otherwise the terminal line discipline edits or discards typeahead before
    // the prompt editor can see it. No reader competes with the prompt editor.
    let mut terminal = if interactive {
        Some(input::Terminal::enter()?)
    } else {
        None
    };
    let mut history = Vec::new();
    if session.store.is_some() {
        preview(&session);
    }
    loop {
        let input = if interactive {
            eprintln!();
            let input = match input::read_line_with_history("> ", &mut history) {
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
                        preview(&session);
                        continue;
                    }
                    match Store::load(&root, &workspace.root, &id)
                        .and_then(|(saved, provider)| Ok((saved, provider, connect(provider)?)))
                    {
                        Ok((saved, saved_provider, saved_client)) => {
                            session = saved;
                            provider = saved_provider;
                            client = saved_client;
                            preview(&session);
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
        let result = turn_saved(
            &mut session.request,
            &workspace,
            max_turns,
            max_context_bytes,
            |req| {
                let styled = styles_enabled(io::stdout().is_terminal());
                let mut stream = StreamDisplay::new(styled);
                let result = client.run_cancellable(req, &INTERRUPTED, |event| {
                    if check_interrupt().is_err() {
                        return false;
                    }
                    let chunk = stream.delta(&event);
                    if !chunk.is_empty() {
                        print!("{chunk}");
                        return io::stdout().flush().is_ok();
                    }
                    true
                });
                print!("{}", stream.finish());
                io::stdout().flush()?;
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
        if checkpoint_failed {
            return result;
        }
        if interactive && INTERRUPTED.swap(false, Ordering::Relaxed) {
            session.request.messages.push(text("user", "Previous turn cancelled. Completed tool effects may remain; inspect state before retrying anything.".into()));
            session.save(false)?;
            eprintln!("cancelled; completed changes were not rolled back");
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

fn preview(session: &Session) {
    if let Some(store) = &session.store {
        eprintln!("Session: {}", store.id);
    }
    for message in session
        .request
        .messages
        .iter()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        for block in &message.content {
            if let Some(text) = block["text"].as_str() {
                let excerpt: String = text.chars().take(240).collect();
                eprintln!(
                    "{}: {}",
                    super::display(&message.role),
                    super::display(&excerpt)
                );
            }
        }
    }
}
