//! Frontend-independent session construction and turn execution.
//! Each process runs one turn; process boundaries isolate cancellation/approvals.
use crate::{
    auth,
    compaction::Automatic,
    engine::{self, Workspace, tools},
    session::{self, Session, Store},
    skills,
};
use llm_api::{Cancelled, Client, CodexQuota, Provider, Request, Response, Result};
use serde_json::json;
use std::{
    cell::RefCell,
    fs::File,
    io,
    os::fd::AsRawFd,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub(crate) fn connect(provider: Provider) -> Result<Client> {
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

pub(crate) fn create(workspace: &Workspace, provider: Provider, model: String) -> Result<Session> {
    if model.is_empty() {
        return Err("model must not be empty".into());
    }
    let mut request = Request {
        model,
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
        .push_str(&skills::catalog(&skills::discover(workspace)));
    if workspace.root.join("AGENTS.md").exists() {
        request.system.push_str(&format!(
            "\nRoot AGENTS.md:\n{}",
            workspace.read(&workspace.path("AGENTS.md")?)?
        ));
    }
    let store = Store::create(&session::directory()?, &workspace.root, provider, &request)?;
    Ok(Session {
        request,
        store: Some(store),
    })
}

/// Cooperative checkout lease, released on close/crash. Lock the directory inode
/// so aliases resolve to the same lock without adding files to the checkout.
/// Independent worktrees have independent directory inodes and can run in parallel.
pub(crate) struct WorkspaceLease {
    _directory: File,
}
impl WorkspaceLease {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        Self::try_acquire(root)?.ok_or_else(|| "workspace busy: another agent turn owns this checkout; wait or select a separate directory/Git worktree".into())
    }

    fn try_acquire(root: &Path) -> Result<Option<Self>> {
        let directory = File::open(root)?;
        if !directory.metadata()?.is_dir() {
            return Err("workspace must be a directory".into());
        }
        // SAFETY: directory is a live owned descriptor. Nonblocking, advisory lock.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::WouldBlock {
                Ok(None)
            } else {
                Err(error.into())
            };
        }
        Ok(Some(Self {
            _directory: directory,
        }))
    }
}

pub(crate) struct TurnModel<'a> {
    client: &'a mut Client,
    store: Option<&'a Store>,
    reload: fn(Provider) -> Result<Client>,
}

impl<'a> TurnModel<'a> {
    pub(crate) fn new(client: &'a mut Client, store: Option<&'a Store>) -> Self {
        Self::with_connector(client, store, connect)
    }

    pub(crate) fn with_connector(
        client: &'a mut Client,
        store: Option<&'a Store>,
        reload: fn(Provider) -> Result<Client>,
    ) -> Self {
        Self {
            client,
            store,
            reload,
        }
    }
}

struct ActiveTurn<'a> {
    model: TurnModel<'a>,
    workspace: &'a Workspace,
    lease: Option<WorkspaceLease>,
    recovery_pending: bool,
}

fn wait_until(
    deadline: Instant,
    cancelled: &AtomicBool,
    check_frontend: impl Fn() -> Result<()>,
) -> Result<()> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        check_frontend()?;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(());
        };
        std::thread::sleep(remaining.min(Duration::from_millis(100)));
    }
}

impl ActiveTurn<'_> {
    fn pause(&mut self, request: &Request, quota: &CodexQuota) -> Result<()> {
        let reported = quota.reset_after();
        let delay = reported.unwrap_or(Duration::from_secs(5 * 3600));
        let deadline = Instant::now() + delay;
        if let Some(store) = self.model.store {
            store.save(request, false)?;
        }
        self.lease.take();
        self.recovery_pending = true;
        let timing = if reported.is_some() {
            format!(
                "provider reset in {}h {:02}m {:02}s",
                delay.as_secs() / 3600,
                delay.as_secs() / 60 % 60,
                delay.as_secs() % 60,
            )
        } else {
            "retrying in 5 hours (fallback; provider did not report a reset)".into()
        };
        self.workspace.frontend.quota_status(
            "quota_paused",
            &format!("Codex subscription usage limit reached. Paused: {timing}. The pending model call will retry automatically; completed tools won't replay."),
        )?;
        let check_frontend = || self.workspace.frontend.quota_check();
        wait_until(deadline, &engine::INTERRUPTED, check_frontend)?;
        // Discord reserves the checkout in its scheduler before Rust reacquires it.
        self.workspace.frontend.quota_ready()?;
        loop {
            engine::check_interrupt()?;
            check_frontend()?;
            if let Some(lease) = WorkspaceLease::try_acquire(&self.workspace.root)? {
                self.lease = Some(lease);
                break;
            }
            wait_until(
                Instant::now() + Duration::from_millis(250),
                &engine::INTERRUPTED,
                check_frontend,
            )?;
        }
        engine::check_interrupt()?;
        *self.model.client = match (self.model.reload)(Provider::Codex) {
            Ok(client) => client,
            Err(_) => {
                let message = "Could not reload the Codex login after the quota wait. Renew it through its login owner before continuing. Saved context and completed tool results were retained.";
                self.workspace
                    .frontend
                    .quota_status("quota_login", message)?;
                return Err(message.into());
            }
        };
        if let Some(store) = self.model.store {
            store.save(request, true)?;
        }
        engine::check_interrupt()?;
        self.workspace.frontend.quota_status(
            "quota_retrying",
            "Retrying the pending model call to check whether Codex usage is back.",
        )
    }

    fn confirm_recovery(&mut self) -> Result<()> {
        if self.recovery_pending {
            engine::check_interrupt()?;
            self.workspace.frontend.quota_check()?;
            self.workspace.frontend.quota_status(
                "quota_resumed",
                "A model call succeeded; continuing the turn.",
            )?;
            self.recovery_pending = false;
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn turn(
    request: &mut Request,
    workspace: &Workspace,
    max_turns: usize,
    max_context_bytes: usize,
    model: TurnModel<'_>,
    mut compact: impl FnMut(&Client, &mut Request) -> Result<Automatic>,
    mut infer: impl FnMut(&Client, &Request) -> Result<Response>,
    checkpoint: impl FnMut(&Request) -> Result<()>,
) -> Result<()> {
    let state = RefCell::new(ActiveTurn {
        model,
        workspace,
        lease: Some(WorkspaceLease::acquire(&workspace.root)?),
        recovery_pending: false,
    });
    engine::turn_compacting(
        request,
        workspace,
        max_turns,
        max_context_bytes,
        |request| loop {
            engine::check_interrupt()?;
            let mut state = state.borrow_mut();
            match compact(state.model.client, request) {
                Err(error) if error.is::<CodexQuota>() => {
                    state.pause(request, error.downcast_ref::<CodexQuota>().unwrap())?;
                }
                Ok(Automatic::Compacted) => {
                    state.confirm_recovery()?;
                    return Ok(true);
                }
                Ok(Automatic::Failed) => return Ok(true),
                Ok(Automatic::NotNeeded) => return Ok(false),
                Err(error) => return Err(error),
            }
        },
        |settled, projected| loop {
            engine::check_interrupt()?;
            let mut state = state.borrow_mut();
            match infer(state.model.client, projected) {
                Err(error) if error.is::<CodexQuota>() => {
                    state.pause(settled, error.downcast_ref::<CodexQuota>().unwrap())?;
                }
                Ok(response) => {
                    if matches!(response.finish_reason.as_str(), "stop" | "tool_calls") {
                        state.confirm_recovery()?;
                    }
                    return Ok(response);
                }
                Err(error) => return Err(error),
            }
        },
        checkpoint,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, request, response};

    #[test]
    fn checkout_lease_excludes_aliases_and_releases_on_drop() {
        let first = Fixture::new();
        let second = Fixture::new();
        let lease = WorkspaceLease::acquire(&first.0.root).unwrap();
        assert!(WorkspaceLease::acquire(&first.0.root.join(".")).is_err());
        assert!(WorkspaceLease::acquire(&second.0.root).is_ok());
        drop(lease);
        assert!(WorkspaceLease::acquire(&first.0.root).is_ok());
    }

    #[test]
    fn busy_checkout_never_calls_model_and_error_releases_lease() {
        let fixture = Fixture::new();
        let lease = WorkspaceLease::acquire(&fixture.0.root).unwrap();
        let result = turn(
            &mut request(),
            &fixture.0,
            1,
            1024 * 1024,
            TurnModel::new(&mut Client::new(Provider::Openai, "dummy"), None),
            |_, _| panic!("busy checkout must not compact"),
            |_, _| panic!("busy checkout must not infer"),
            |_| Ok(()),
        );
        assert!(result.unwrap_err().to_string().contains("workspace busy"));
        drop(lease);
        assert!(
            turn(
                &mut request(),
                &fixture.0,
                1,
                1024 * 1024,
                TurnModel::new(&mut Client::new(Provider::Openai, "dummy"), None),
                |_, _| Ok(Automatic::NotNeeded),
                |_, _| Err("synthetic provider failure".into()),
                |_| Ok(())
            )
            .is_err()
        );
        turn(
            &mut request(),
            &fixture.0,
            1,
            1024 * 1024,
            TurnModel::new(&mut Client::new(Provider::Openai, "dummy"), None),
            |_, _| Ok(Automatic::NotNeeded),
            |_, _| Ok(response(vec![], "stop")),
            |_| Ok(()),
        )
        .unwrap();
    }
}
