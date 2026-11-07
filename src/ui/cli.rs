//! CLI configuration and terminal conversation controller.
use super::{StreamDisplay, TerminalFrontend, input, styles_enabled};
use crate::{
    INTERRUPTED, Workspace, auth, check_interrupt,
    engine::{text, tools, turn},
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
                    "agent [--provider openai|anthropic|openrouter|codex] [--model MODEL] [--ask|--yes] [--verbose] [--list-skills] [--max-turns N] [--max-context-kib N] [--] [PROMPT...]\nDefaults: provider codex, model gpt-6.1-sol (overridable with AGENT_PROVIDER/AGENT_MODEL).\nLimits: 500 model calls per prompt, 4096 KiB serialized context (not a token limit; provider limits still apply).\nNo prompt: interactive conversation. /quit exits, /clear resets history.\nTool calls are allowed without confirmation by default, including unsandboxed shell commands with full user privileges.\n--ask requires confirmation for shell commands and file writes; --yes restores automatic approval.\nTool activity is compact by default; --verbose shows full arguments and bounded results."
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
    let client = match provider {
        Provider::Codex => auth::codex_client()?,
        _ => {
            let key = match provider {
                Provider::Anthropic => "ANTHROPIC_API_KEY",
                Provider::Openrouter => "OPENROUTER_API_KEY",
                _ => "OPENAI_API_KEY",
            };
            Client::new(provider, std::env::var(key)?)
        }
    };
    let workspace = Workspace {
        root: std::env::current_dir()?.canonicalize()?,
        yes,
        frontend: Box::new(TerminalFrontend { verbose }),
    };
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
    chat(
        client,
        workspace,
        request,
        prompt,
        max_turns,
        max_context_bytes,
    )
}

pub(crate) fn chat(
    client: Client,
    workspace: Workspace,
    mut request: Request,
    prompt: Vec<String>,
    max_turns: usize,
    max_context_bytes: usize,
) -> Result<()> {
    let interactive = prompt.is_empty();
    if interactive && !io::stdin().is_terminal() {
        return Err("provide a prompt argument for noninteractive use".into());
    }
    loop {
        let input = if interactive {
            eprintln!();
            let input = match input::read_line("> ") {
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
                    request.messages.clear();
                    continue;
                }
                "" => continue,
                _ => input,
            }
        } else {
            prompt.join(" ")
        };
        request.messages.push(text("user", input));
        let result = turn(
            &mut request,
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
        );
        if interactive && INTERRUPTED.swap(false, Ordering::Relaxed) {
            request.messages.push(text("user", "Previous turn cancelled. Completed tool effects may remain; inspect state before retrying anything.".into()));
            eprintln!("cancelled; completed changes were not rolled back");
            continue;
        }
        result?;
        if !interactive {
            break;
        }
    }
    Ok(())
}
