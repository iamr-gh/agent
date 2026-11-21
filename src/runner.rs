//! Frontend-independent session construction and turn execution.
//! Each process runs one turn; process boundaries isolate cancellation/approvals.
use crate::{
    auth,
    engine::{self, Workspace, tools},
    session::{self, Session, Store},
    skills,
};
use llm_api::{Client, Provider, Request, Response, Result};
use serde_json::json;
use std::{fs::File, os::fd::AsRawFd, path::Path};

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
        let directory = File::open(root)?;
        if !directory.metadata()?.is_dir() {
            return Err("workspace must be a directory".into());
        }
        // SAFETY: directory is a live owned descriptor. Nonblocking, advisory lock.
        if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("workspace busy: another agent turn owns this checkout; wait or select a separate directory/Git worktree".into());
        }
        Ok(Self {
            _directory: directory,
        })
    }
}

pub(crate) fn turn(
    request: &mut Request,
    workspace: &Workspace,
    max_turns: usize,
    max_context_bytes: usize,
    compact: impl FnMut(&mut Request) -> Result<bool>,
    infer: impl FnMut(&Request) -> Result<Response>,
    checkpoint: impl FnMut(&Request) -> Result<()>,
) -> Result<()> {
    let _lease = WorkspaceLease::acquire(&workspace.root)?;
    engine::turn_compacting(
        request,
        workspace,
        max_turns,
        max_context_bytes,
        compact,
        infer,
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
            |_| panic!("busy checkout must not compact"),
            |_| panic!("busy checkout must not infer"),
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
                |_| Ok(false),
                |_| Err("synthetic provider failure".into()),
                |_| Ok(())
            )
            .is_err()
        );
        turn(
            &mut request(),
            &fixture.0,
            1,
            1024 * 1024,
            |_| Ok(false),
            |_| Ok(response(vec![], "stop")),
            |_| Ok(()),
        )
        .unwrap();
    }
}
