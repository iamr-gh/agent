//! Frontend-neutral JSON-lines subprocess protocol (legacy filename).
use crate::{
    compaction,
    engine::{Frontend, INTERRUPTED, Workspace, text},
    runner,
    session::{self, Store},
};
use llm_api::{Provider, Result};
use serde_json::{Value, json};
use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

fn emit(value: Value) -> Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, &value)?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

fn read_line(input: &mut impl BufRead) -> Result<Option<Value>> {
    let mut line = String::new();
    if input.take(1024 * 1024).read_line(&mut line)? == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        return Err("unterminated or oversized adapter message".into());
    }
    Ok(Some(serde_json::from_str(&line)?))
}

// Only this thread blocks on the pipe. The worker can notice Ctrl-C even when
// the adapter keeps stdin open without sending an approval or resume grant.
fn adapter_input() -> Receiver<Result<Value>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut input = io::stdin().lock();
        loop {
            match read_line(&mut input) {
                Ok(Some(value)) => {
                    if sender.send(Ok(value)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = sender.send(Err(error));
                    break;
                }
            }
        }
    });
    receiver
}

fn is_approval(answer: &Value) -> bool {
    answer == &json!({"approve":true})
}

fn policy(input: &Value) -> Result<(bool, usize)> {
    let auto_approve = match input.get("auto_approve") {
        None => false,
        Some(value) => value.as_bool().ok_or("auto_approve must be a boolean")?,
    };
    let max_turns = match input.get("max_turns") {
        None => 25,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=100).contains(n))
            .ok_or("max_turns must be an integer from 1 to 100")? as usize,
    };
    Ok((auto_approve, max_turns))
}

struct ProtocolFrontend(Receiver<Result<Value>>);
impl ProtocolFrontend {
    fn receive(&self) -> Result<Value> {
        loop {
            crate::check_interrupt()?;
            match self.0.recv_timeout(Duration::from_millis(100)) {
                Ok(value) => return value,
                Err(RecvTimeoutError::Timeout) => (),
                Err(RecvTimeoutError::Disconnected) => return Err("adapter disconnected".into()),
            }
        }
    }
}
impl Frontend for ProtocolFrontend {
    fn approve(&self, name: &str, args: &Value) -> Result<()> {
        emit(json!({"type":"approval", "name":name, "args":args}))?;
        let answer = self.receive()?;
        if is_approval(&answer) {
            Ok(())
        } else {
            Err("denied by frontend or approval expired".into())
        }
    }
    fn tool_started(&self, _: usize, name: &str, _: &Value) {
        let _ = emit(json!({"type":"tool", "name":name}));
    }
    fn tool_finished(&self, index: usize, name: &str, _: &str, error: bool, elapsed: Duration) {
        let _ = emit(json!({"type":"tool_finished", "index":index, "name":name,
            "error":error, "elapsed_ms":elapsed.as_millis()}));
    }
    fn quota_status(&self, state: &str, message: &str) -> Result<()> {
        emit(json!({"type":state, "message":message}))
    }
    fn quota_ready(&self) -> Result<()> {
        emit(json!({"type":"quota_ready"}))?;
        if self.receive()? != json!({"resume":true}) {
            return Err("adapter did not grant checkout access".into());
        }
        Ok(())
    }
    fn quota_check(&self) -> Result<()> {
        match self.0.try_recv() {
            Err(TryRecvError::Empty) => Ok(()),
            Err(TryRecvError::Disconnected) => Err("adapter disconnected".into()),
            Ok(Err(error)) => Err(error),
            Ok(Ok(_)) => Err("unexpected adapter message while paused".into()),
        }
    }
    fn diagnostic(&self, message: &str) {
        eprintln!("{message}");
    }
}

pub(crate) fn run() -> Result<()> {
    run_with_connector(runner::connect)
}

fn run_with_connector(connect: fn(Provider) -> Result<llm_api::Client>) -> Result<()> {
    let frontend = ProtocolFrontend(adapter_input());
    let input = frontend.receive()?;
    let prompt = input["prompt"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .ok_or("missing prompt")?;
    let (auto_approve, max_turns) = policy(&input)?;
    let workspace = Workspace {
        root: std::env::current_dir()?.canonicalize()?,
        yes: auto_approve,
        frontend: Box::new(frontend),
    };
    let root = session::directory()?;
    let (mut session, provider) = if let Some(id) = input["session"].as_str() {
        Store::load(&root, &workspace.root, id)?
    } else {
        let provider: Provider = serde_json::from_value(json!(
            std::env::var("AGENT_PROVIDER").unwrap_or("codex".into())
        ))?;
        (
            runner::create(
                &workspace,
                provider,
                std::env::var("AGENT_MODEL").unwrap_or(crate::models::DEFAULT_MODEL.into()),
            )?,
            provider,
        )
    };
    let mut client = connect(provider)?;
    emit(json!({"type":"session", "id":session.store.as_ref().unwrap().id}))?;
    // Presentation belongs to the adapter, not persisted core instructions.
    let presentation = input
        .get("presentation")
        .and_then(Value::as_str)
        .unwrap_or("");
    session.request.messages.push(text("user", prompt.into()));
    let threshold = compaction::threshold()?;
    session.save(true)?;
    let store = session.store.as_ref().unwrap();
    let result = runner::turn(
        &mut session.request,
        &workspace,
        max_turns,
        4 * 1024 * 1024,
        runner::TurnModel::with_connector(&mut client, Some(store), connect),
        compaction::automatic(Some(store), threshold, |_| {}),
        |client, request| {
            let mut presented = request.clone();
            presented.system.push_str(presentation);
            let response = client.run_cancellable(&presented, &INTERRUPTED, |_| true)?;
            for block in &response.message.content {
                if response.finish_reason == "stop" && block["type"] == "text" {
                    emit(json!({"type":"answer", "text":block["text"]}))?;
                }
            }
            Ok(response)
        },
        |request| store.save(request, true),
    );
    session.save(false)?;
    if let Err(error) = result {
        if error.to_string() != "turn limit reached; stopped without another model request" {
            return Err(error);
        }
        emit(json!({"type":"limit", "max_turns":max_turns}))?;
    }
    emit(json!({"type":"done"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_worker() {
        if std::env::var_os("AGENT_TEST_WORKER_PORT").is_none() {
            return;
        }
        crate::install_interrupt_handler();
        let result = run_with_connector(|provider| {
            assert_eq!(provider, Provider::Codex);
            let port: u16 = std::env::var("AGENT_TEST_WORKER_PORT")?.parse()?;
            llm_api::Client::codex("synthetic-token", "synthetic-account")?
                .with_endpoint(format!("http://127.0.0.1:{port}/responses"))
        });
        if let Err(error) = &result {
            eprintln!("worker: {error}");
        }
        std::process::exit(if INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            130
        } else {
            i32::from(result.is_err())
        });
    }

    #[test]
    fn worker_policy_defaults_fail_closed_and_bounds_turns() {
        assert_eq!(policy(&json!({})).unwrap(), (false, 25));
        assert_eq!(
            policy(&json!({"auto_approve":true, "max_turns":1})).unwrap(),
            (true, 1)
        );
        assert_eq!(
            policy(&json!({"auto_approve":false, "max_turns":100})).unwrap(),
            (false, 100)
        );
        for input in [
            json!({"auto_approve":"true"}),
            json!({"auto_approve":1}),
            json!({"max_turns":0}),
            json!({"max_turns":101}),
            json!({"max_turns":-1}),
            json!({"max_turns":1.5}),
            json!({"max_turns":"25"}),
        ] {
            assert!(policy(&input).is_err());
        }
    }
    #[test]
    fn automatic_shell_tools_stop_at_the_model_call_cap() {
        let (yes, max_turns) = policy(&json!({"auto_approve":true, "max_turns":2})).unwrap();
        let workspace = Workspace {
            root: std::env::current_dir().unwrap(),
            yes,
            frontend: Box::new(ProtocolFrontend(mpsc::channel().1)),
        };
        let mut request = crate::tests::request();
        let mut calls = 0;
        let error = crate::engine::turn_saved(
            &mut request,
            &workspace,
            max_turns,
            1024 * 1024,
            |_| {
                calls += 1;
                Ok(crate::tests::response(
                    vec![crate::tests::call(
                        &calls.to_string(),
                        "shell",
                        json!({"command":"printf ok"}),
                    )],
                    "tool_calls",
                ))
            },
            |_| Ok(()),
        )
        .unwrap_err();
        assert_eq!(calls, 2);
        assert_eq!(
            error.to_string(),
            "turn limit reached; stopped without another model request"
        );
        assert!(
            request
                .messages
                .last()
                .unwrap()
                .content
                .iter()
                .all(|block| block["type"] == "tool_result" && block["is_error"] == false)
        );
    }

    #[test]
    fn manual_approval_requires_an_exact_boolean_grant() {
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
