//! Atomic context snapshots, not terminal logs. Loading never executes saved calls.
use crate::engine::{atomic_write, text, tools};
use llm_api::{Provider, Request, Result};
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) struct Session {
    pub request: Request,
    pub store: Option<Store>,
}

pub(crate) struct Summary {
    pub id: String,
    pub title: String,
    pub model: String,
    pub modified: SystemTime,
}

pub(crate) struct Store {
    pub id: String,
    root: PathBuf,
    workspace: PathBuf,
    provider: Provider,
    _lock: File,
}

pub(crate) fn directory() -> Result<PathBuf> {
    let home = match std::env::var_os("AGENT_HOME") {
        Some(home) => PathBuf::from(home),
        None => PathBuf::from(std::env::var_os("HOME").ok_or("HOME is required")?).join(".agent"),
    };
    let root = home.join("sessions");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&root)?;
    private(&root, true)?;
    Ok(root)
}

fn private(path: &Path, directory: bool) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    // SAFETY: getuid only reads the real user ID.
    if meta.uid() != unsafe { libc::getuid() }
        || meta.mode() & 0o077 != 0
        || if directory {
            !meta.is_dir()
        } else {
            !meta.is_file()
        }
    {
        return Err("session storage must be private, user-owned, and not a symlink".into());
    }
    Ok(())
}

impl Store {
    fn open(root: &Path, workspace: &Path, provider: Provider, id: &str) -> Result<Self> {
        if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err("invalid session ID".into());
        }
        private(root, true)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(root.join(format!("{id}.lock")))?;
        private(&root.join(format!("{id}.lock")), false)?;
        // SAFETY: lock is an owned live descriptor; flock is released on close/exec.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("session is already open in another process".into());
        }
        Ok(Self {
            id: id.into(),
            root: root.into(),
            workspace: workspace.into(),
            provider,
            _lock: lock,
        })
    }

    pub fn create(
        root: &Path,
        workspace: &Path,
        provider: Provider,
        request: &Request,
    ) -> Result<Self> {
        let id = format!(
            "{}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id()
        );
        let store = Self::open(root, workspace, provider, &id)?;
        store.save(request, false)?;
        Ok(store)
    }

    pub fn save(&self, request: &Request, active: bool) -> Result<()> {
        let data = json!({"version":1, "workspace":self.workspace, "provider":self.provider,
            "request":request, "active":active});
        atomic_write(
            &self.root.join(format!("{}.json", self.id)),
            &serde_json::to_string(&data)?,
        )?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub fn load(root: &Path, workspace: &Path, id: &str) -> Result<(Session, Provider)> {
        // Validate the ID before constructing any path.
        let mut store = Self::open(root, workspace, Provider::Openai, id)?;
        let path = root.join(format!("{id}.json"));
        private(&path, false)?;
        let data: Value = serde_json::from_slice(&fs::read(path)?)?;
        if data["version"] != 1 || data["workspace"] != json!(workspace) {
            return Err("unsupported session version or different workspace".into());
        }
        let provider: Provider = serde_json::from_value(data["provider"].clone())?;
        let mut request: Request = serde_json::from_value(data["request"].clone())?;
        if request.tools != tools() {
            return Err(
                "saved tool schemas differ from this executable; cannot safely resume".into(),
            );
        }
        if data["active"].as_bool().ok_or("missing session state")? {
            request.messages.push(text("user", "Previous turn was interrupted. Saved tool results are retained. Unfinished tools were not replayed; their effects may remain. Inspect workspace state before retrying.".into()));
        }
        store.provider = provider;
        store.save(&request, false)?;
        Ok((
            Session {
                request,
                store: Some(store),
            },
            provider,
        ))
    }

    pub fn list(root: &Path, workspace: &Path) -> Result<Vec<Summary>> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            private(&path, false)?;
            let data: Value = serde_json::from_slice(&fs::read(&path)?)?;
            if data["workspace"] != json!(workspace) {
                continue;
            }
            let title = data["request"]["messages"]
                .as_array()
                .and_then(|messages| messages.iter().find(|m| m["role"] == "user"))
                .and_then(|m| m["content"][0]["text"].as_str())
                .unwrap_or("(empty)");
            sessions.push(Summary {
                id: path.file_stem().unwrap().to_string_lossy().into_owned(),
                title: title.chars().take(240).collect(),
                model: data["request"]["model"]
                    .as_str()
                    .unwrap_or("unknown model")
                    .into(),
                modified: fs::metadata(&path)?.modified()?,
            });
        }
        sessions.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| b.id.cmp(&a.id)));
        Ok(sessions)
    }
}

impl Session {
    pub fn save(&self, active: bool) -> Result<()> {
        if let Some(store) = &self.store {
            store.save(&self.request, active)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        engine::turn_saved,
        tests::{Fixture, call, request, response},
    };
    use std::os::unix::fs::PermissionsExt;

    fn root(f: &Fixture) -> PathBuf {
        let root = f.0.root.join("sessions");
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        root
    }

    #[test]
    fn round_trip_preserves_full_context_and_private_permissions() {
        let f = Fixture::new();
        let root = root(&f);
        let mut req = request();
        req.system = "saved instructions".into();
        req.options = json!({"reasoning":{"summary":"auto"}})
            .as_object()
            .unwrap()
            .clone();
        req.messages
            .push(response(vec![json!({"type":"text","text":"answer"})], "stop").message);
        let expected = serde_json::to_value(&req).unwrap();
        let store = Store::create(&root, &f.0.root, Provider::Openai, &req).unwrap();
        let id = store.id.clone();
        assert_eq!(
            fs::metadata(root.join(format!("{id}.json")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // SAFETY: querying flags of an owned descriptor does not mutate it.
        assert_ne!(
            unsafe { libc::fcntl(store._lock.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert!(Store::load(&root, &f.0.root, &id).is_err());
        drop(store);
        let (loaded, provider) = Store::load(&root, &f.0.root, &id).unwrap();
        assert_eq!(provider, Provider::Openai);
        assert_eq!(serde_json::to_value(&loaded.request).unwrap(), expected);
        assert_eq!(Store::list(&root, &f.0.root).unwrap().len(), 1);
        assert!(Store::list(&root, &root).unwrap().is_empty());
    }

    #[test]
    fn summaries_show_prompt_and_model_in_last_modified_order() {
        let f = Fixture::new();
        let root = root(&f);
        let mut req = request();
        req.messages = vec![text("user", "Fix the parser".into())];
        let first = Store::create(&root, &f.0.root, Provider::Openai, &req).unwrap();
        req.messages = vec![text("user", "Add tests".into())];
        let second = Store::create(&root, &f.0.root, Provider::Openai, &req).unwrap();
        // An older-created chat with newer activity belongs first.
        File::options()
            .write(true)
            .open(root.join(format!("{}.json", second.id)))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH))
            .unwrap();
        let entries = Store::list(&root, &f.0.root).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, first.id);
        assert_eq!(entries[0].title, "Fix the parser");
        assert_eq!(entries[0].model, req.model);
        assert_eq!(entries[1].id, second.id);
        assert_eq!(entries[1].title, "Add tests");
        assert_eq!(entries[1].modified, UNIX_EPOCH);
    }

    #[test]
    fn crash_between_tools_retains_results_and_never_replays_effects() {
        let f = Fixture::new();
        let root = root(&f);
        let mut req = request();
        let store = Store::create(&root, &f.0.root, Provider::Openai, &req).unwrap();
        let id = store.id.clone();
        let mut checkpoints = 0;
        let result = turn_saved(
            &mut req,
            &f.0,
            2,
            4 * 1024 * 1024,
            |_| {
                Ok(response(
                    vec![
                        call("a", "shell", json!({"command":"printf x >> effect"})),
                        call(
                            "b",
                            "write_file",
                            json!({"path":"pending", "content":"bad"}),
                        ),
                        call("c", "write_file", json!({"path":"later", "content":"bad"})),
                    ],
                    "tool_calls",
                ))
            },
            |req| {
                store.save(req, true)?;
                checkpoints += 1;
                if checkpoints == 4 {
                    return Err("simulated crash after durable intent".into());
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        drop(store);
        let (loaded, _) = Store::load(&root, &f.0.root, &id).unwrap();
        assert_eq!(
            loaded.request.messages[1].replay,
            Some(json!({"opaque":"keep"}))
        );
        let results = &loaded.request.messages[2].content;
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["is_error"], false);
        assert!(results[1]["output"].as_str().unwrap().contains("unknown"));
        assert!(
            results[2]["output"]
                .as_str()
                .unwrap()
                .contains("not executed")
        );
        assert_eq!(fs::read_to_string(f.0.root.join("effect")).unwrap(), "x");
        assert!(!f.0.root.join("pending").exists());
        assert!(!f.0.root.join("later").exists());
        assert_eq!(loaded.request.messages.len(), 4);
        drop(loaded);
        let (again, _) = Store::load(&root, &f.0.root, &id).unwrap();
        assert_eq!(
            again.request.messages.len(),
            4,
            "recovery notice is not duplicated"
        );
        assert_eq!(fs::read_to_string(f.0.root.join("effect")).unwrap(), "x");
    }

    #[test]
    fn checkpoint_failure_prevents_all_tool_effects() {
        let f = Fixture::new();
        let result = turn_saved(
            &mut request(),
            &f.0,
            2,
            1024 * 1024,
            |_| {
                Ok(response(
                    vec![call(
                        "a",
                        "write_file",
                        json!({"path":"effect", "content":"bad"}),
                    )],
                    "tool_calls",
                ))
            },
            |_| Err("disk full".into()),
        );
        assert!(result.unwrap_err().to_string().contains("disk full"));
        assert!(!f.0.root.join("effect").exists());
    }

    #[test]
    fn rejects_bad_versions_workspaces_schemas_ids_and_corruption() {
        let f = Fixture::new();
        let root = root(&f);
        let store = Store::create(&root, &f.0.root, Provider::Openai, &request()).unwrap();
        let id = store.id.clone();
        let path = root.join(format!("{id}.json"));
        let original: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        drop(store);
        assert!(Store::load(&root, &root, &id).is_err());
        for field in ["version", "request"] {
            let mut data = original.clone();
            data[field] = json!(99);
            fs::write(&path, data.to_string()).unwrap();
            assert!(Store::load(&root, &f.0.root, &id).is_err());
        }
        let mut data = original;
        data["request"]["tools"] = json!([]);
        fs::write(&path, data.to_string()).unwrap();
        assert!(Store::load(&root, &f.0.root, &id).is_err());
        fs::write(&path, "{truncated").unwrap();
        assert!(Store::load(&root, &f.0.root, &id).is_err());
        assert!(Store::load(&root, &f.0.root, "../escape").is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Store::load(&root, &f.0.root, &id).is_err());
    }
}
