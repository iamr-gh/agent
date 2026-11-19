//! Private JSON-lines subprocess protocol for the Discord adapter.
use crate::{
    auth,
    engine::{Frontend, INTERRUPTED, Workspace, text, tools, turn_saved},
    session::{self, Session, Store},
    skills,
};
use llm_api::{Client, Provider, Request, Result};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::time::Duration;

fn emit(value: Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn receive() -> Result<Value> {
    let mut line = String::new();
    if io::stdin().lock().take(1024 * 1024).read_line(&mut line)? == 0 {
        return Err("adapter disconnected".into());
    }
    Ok(serde_json::from_str(&line)?)
}

fn is_approval(answer: &Value) -> bool {
    answer == &json!({"approve":true})
}

struct DiscordFrontend;
impl Frontend for DiscordFrontend {
    fn approve(&self, name: &str, args: &Value) -> Result<()> {
        emit(json!({"type":"approval", "name":name, "args":args}))?;
        let answer = receive()?;
        if is_approval(&answer) {
            Ok(())
        } else {
            Err("denied by Discord user or approval expired".into())
        }
    }
    fn tool_started(&self, _: usize, name: &str, _: &Value) {
        let _ = emit(json!({"type":"tool", "name":name}));
    }
    fn tool_finished(&self, _: usize, _: &str, _: &str, _: bool, _: Duration) {}
    fn diagnostic(&self, _: &str) {}
}

pub(crate) fn run() -> Result<()> {
    let input = receive()?;
    let prompt = input["prompt"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing prompt")?;
    let workspace = Workspace {
        root: std::env::current_dir()?.canonicalize()?,
        yes: false,
        frontend: Box::new(DiscordFrontend),
    };
    let root = session::directory()?;
    let (mut session, provider) = if let Some(id) = input["session"].as_str() {
        Store::load(&root, &workspace.root, id)?
    } else {
        let provider: Provider = serde_json::from_value(json!(
            std::env::var("AGENT_PROVIDER").unwrap_or("codex".into())
        ))?;
        let mut request = Request {
            model: std::env::var("AGENT_MODEL").unwrap_or("gpt-6.1-sol".into()),
            messages: Vec::new(),
            system: format!(
                "{}\nWorkspace: {}",
                include_str!("prompt.txt"),
                workspace.root.display()
            ),
            tools: tools(),
            json_schema: None,
            max_tokens: 8192,
            options: if provider == Provider::Codex {
                serde_json::from_value(json!({"reasoning":{"summary":"auto"}}))?
            } else {
                Default::default()
            },
        };
        request
            .system
            .push_str(&skills::catalog(&skills::discover(&workspace)));
        if workspace.root.join("AGENTS.md").exists() {
            request.system.push_str(&format!(
                "\nRoot AGENTS.md:\n{}",
                workspace.read(&workspace.path("AGENTS.md")?)?
            ));
        }
        let store = Store::create(&root, &workspace.root, provider, &request)?;
        (
            Session {
                request,
                store: Some(store),
            },
            provider,
        )
    };
    let client = if provider == Provider::Codex {
        auth::codex_client()?
    } else {
        let key = match provider {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
            Provider::Openrouter => "OPENROUTER_API_KEY",
            _ => "OPENAI_API_KEY",
        };
        Client::new(provider, std::env::var(key)?)
    };
    emit(json!({"type":"session", "id":session.store.as_ref().unwrap().id}))?;
    session.request.messages.push(text("user", prompt.into()));
    session.save(true)?;
    let store = session.store.as_ref().unwrap();
    turn_saved(
        &mut session.request,
        &workspace,
        100,
        4 * 1024 * 1024,
        |request| {
            let response = client.run_cancellable(request, &INTERRUPTED, |_| true)?;
            for block in &response.message.content {
                if block["type"] == "text" {
                    emit(json!({"type":"answer", "text":block["text"]}))?;
                }
            }
            Ok(response)
        },
        |request| store.save(request, true),
    )?;
    session.save(false)?;
    emit(json!({"type":"done"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_workspace_never_auto_approves() {
        assert!(is_approval(&json!({"approve":true})));
        for answer in [
            json!({"approve":"true"}),
            json!({}),
            json!({"approve":false}),
            json!({"approve":true,"extra":1}),
        ] {
            assert!(!is_approval(&answer));
        }
    }
}
