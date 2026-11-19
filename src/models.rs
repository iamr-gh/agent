//! Best-effort Codex catalog. Disk, authentication and network work stay off the UI thread.
use crate::{auth, engine::atomic_write, session};
use llm_api::Result;
use serde_json::{Value, json};
use std::fs::OpenOptions;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) const DEFAULT_MODEL: &str = "gpt-6.2-sol";
const ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/models";
const VERSION_ENDPOINT: &str = "https://registry.npmjs.org/@openai/codex/latest";
const TTL: u64 = 3600;
const LIMIT: u64 = 1024 * 1024;
type Catalog = Arc<Mutex<Option<Vec<String>>>>;
static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// No join, disk access, authentication or network on the calling thread.
pub(crate) fn start() {
    if std::env::var("AGENT_MODEL_DISCOVERY").as_deref() == Ok("off") {
        return;
    }
    CATALOG.get_or_init(|| {
        let catalog = Arc::new(Mutex::new(None));
        let worker = Arc::clone(&catalog);
        let _ = std::thread::Builder::new()
            .name("codex-models".into())
            .spawn(move || {
                let _ = (|| -> Result<()> {
                    let credentials = auth::codex_credentials()?;
                    let mut key = DefaultHasher::new();
                    credentials.account.hash(&mut key);
                    let path = session::directory()?
                        .join(format!("codex-models-{:016x}.cache", key.finish()));
                    refresh(&worker, &path, || fetch_live(&credentials))
                })(); // Discovery failure must never interrupt chat or print credentials.
            });
        catalog
    });
}

/// A busy worker never blocks the picker. An open picker is a stable snapshot;
/// reopening it picks up any refresh completed in the meantime.
pub(crate) fn discovered() -> Option<Vec<String>> {
    CATALOG.get()?.try_lock().ok()?.clone()
}

fn publish(catalog: &Catalog, ids: Vec<String>) {
    if let Ok(mut models) = catalog.lock() {
        *models = Some(ids);
    }
}

fn refresh(
    catalog: &Catalog,
    path: &Path,
    fetch: impl FnOnce() -> Result<Vec<String>>,
) -> Result<()> {
    if let Ok((ids, fresh)) = read_cache(path) {
        publish(catalog, ids);
        if fresh {
            return Ok(());
        }
    }
    let ids = fetch()?;
    // Publish even if the cache cannot be written. Never replace good data on error.
    publish(catalog, ids.clone());
    atomic_write(
        path,
        &json!({"version":1,"updated":now()?,"ids":ids}).to_string(),
    )
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_graphic())
}

fn read_cache(path: &Path) -> Result<(Vec<String>, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    // SAFETY: getuid only reads the real user ID.
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        return Err("model cache must be a private user-owned regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err("model cache too large".into());
    }
    let data: Value = serde_json::from_slice(&bytes)?;
    if data["version"] != 1 {
        return Err("unsupported model cache version".into());
    }
    let updated = data["updated"].as_u64().ok_or("missing cache timestamp")?;
    let ids = data["ids"].as_array().ok_or("missing cached models")?;
    let mut models = Vec::new();
    for value in ids {
        let id = value
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or("invalid cached model")?;
        if !models.iter().any(|model| model == id) {
            models.push(id.to_owned());
        }
    }
    if models.is_empty() {
        return Err("empty model cache".into());
    }
    let age = now()?.checked_sub(updated);
    Ok((models, age.is_some_and(|age| age < TTL)))
}

fn fetch_live(credentials: &auth::CodexCredentials) -> Result<Vec<String>> {
    // Like fx, use the upstream Codex client version, not ag's package version:
    // the service can return an empty catalog for an old client_version.
    // This public request carries no subscription headers or credentials.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(3)))
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent.get(VERSION_ENDPOINT).call()?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_to_vec()?;
    let data: Value = serde_json::from_slice(&bytes)?;
    let version = client_version(&data)?;
    fetch(&format!("{ENDPOINT}?client_version={version}"), credentials)
}

fn client_version(data: &Value) -> Result<&str> {
    let version = data["version"]
        .as_str()
        .ok_or("missing Codex client version")?;
    let parts: Vec<_> = version.split('.').collect();
    if version.len() > 32
        || parts.len() != 3
        || parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err("invalid Codex client version".into());
    }
    Ok(version)
}

fn fetch(endpoint: &str, credentials: &auth::CodexCredentials) -> Result<Vec<String>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5)))
        .max_redirects(0)
        .build()
        .into();
    let mut response = agent
        .get(endpoint)
        .header("Authorization", format!("Bearer {}", credentials.token))
        .header("chatgpt-account-id", &credentials.account)
        .header("originator", "ag")
        .header("Accept", "application/json")
        .call()?;
    if !response.status().is_success() {
        return Err("model catalog request failed".into());
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(LIMIT)
        .read_to_vec()?;
    parse(&serde_json::from_slice(&bytes)?)
}

fn parse(data: &Value) -> Result<Vec<String>> {
    let models = data["models"].as_array().ok_or("missing model catalog")?;
    let mut ids = Vec::new();
    for model in models {
        if model["visibility"].as_str() != Some("list")
            || model["supported_in_api"].as_bool() != Some(true)
        {
            continue;
        }
        let Some(id) = model["slug"].as_str().filter(|id| valid_id(id)) else {
            continue;
        };
        if !ids.iter().any(|existing| existing == id) {
            ids.push(id.to_owned());
        }
    }
    if ids.is_empty() {
        return Err("empty model catalog".into());
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::Fixture;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    #[ignore = "uses the existing Codex login for a live metadata-only request"]
    fn live_codex_catalog() {
        let credentials =
            auth::codex_credentials().unwrap_or_else(|_| panic!("Codex login unavailable"));
        let ids = fetch_live(&credentials)
            .unwrap_or_else(|error| panic!("catalog fetch failed: {error}"));
        assert!(ids.iter().any(|id| id == DEFAULT_MODEL));
        println!("Live Codex catalog: {} models", ids.len());
    }

    #[test]
    fn validates_upstream_client_version_before_using_it_in_url() {
        assert_eq!(
            client_version(&json!({"version":"0.170.0"})).unwrap(),
            "0.170.0"
        );
        for version in ["", "1", "1.2", "1.2.3&injected=1", "1.2.3\r\n", "1.2.beta"] {
            assert!(client_version(&json!({"version":version})).is_err());
        }
        assert!(client_version(&json!({})).is_err());
    }

    #[test]
    fn validates_catalog_and_filters_hidden_duplicates_and_unsafe_ids() {
        let data = json!({"models":[
            {"slug":"gpt-future","visibility":"list","supported_in_api":true},
            {"slug":"gpt-future","visibility":"list","supported_in_api":true},
            {"slug":"hidden","visibility":"hide","supported_in_api":true},
            {"slug":"unsupported","visibility":"list","supported_in_api":false},
            {"slug":"bad\nmodel","visibility":"list","supported_in_api":true},
            {"slug":"","visibility":"list","supported_in_api":true}, {},
            {"slug":"other","visibility":"list","supported_in_api":true}
        ]});
        assert_eq!(parse(&data).unwrap(), ["gpt-future", "other"]);
        for data in [
            json!({}),
            json!({"models":[]}),
            json!({"models":[{"slug":"bad id"}]}),
        ] {
            assert!(parse(&data).is_err());
        }
    }

    #[test]
    fn cache_is_private_fresh_cache_skips_network_and_stale_survives_failure() {
        let f = Fixture::new();
        let path = f.0.root.join("catalog.cache");
        let catalog = Arc::new(Mutex::new(None));
        refresh(&catalog, &path, || Ok(vec!["gpt-future".into()])).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        refresh(&catalog, &path, || panic!("fresh cache must skip network")).unwrap();
        atomic_write(
            &path,
            &json!({"version":1,"updated":0,"ids":["gpt-stale"]}).to_string(),
        )
        .unwrap();
        assert!(refresh(&catalog, &path, || Err("offline".into())).is_err());
        assert_eq!(catalog.lock().unwrap().as_ref().unwrap(), &["gpt-stale"]);
        assert_eq!(read_cache(&path).unwrap().0, ["gpt-stale"]);
        atomic_write(&path, "corrupt").unwrap();
        refresh(&catalog, &path, || Ok(vec!["recovered".into()])).unwrap();
        assert_eq!(read_cache(&path).unwrap().0, ["recovered"]);
    }

    #[test]
    fn stalled_discovery_does_not_hold_the_catalog_lock() {
        let f = Fixture::new();
        let path = f.0.root.join("catalog.cache");
        let catalog = Arc::new(Mutex::new(None));
        let worker = Arc::clone(&catalog);
        let (started, wait_started) = mpsc::channel();
        let (release, wait_release) = mpsc::channel();
        let task = std::thread::spawn(move || {
            refresh(&worker, &path, || {
                started.send(()).unwrap();
                wait_release.recv().unwrap();
                Ok(vec!["new-model".into()])
            })
        });
        wait_started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(catalog.try_lock().unwrap().is_none());
        release.send(()).unwrap();
        task.join().unwrap().unwrap();
        assert_eq!(
            catalog.try_lock().unwrap().as_ref().unwrap(),
            &["new-model"]
        );
    }

    #[test]
    fn loopback_discovery_uses_subscription_headers_and_parses_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!(
            "http://{}/models?client_version=test",
            listener.local_addr().unwrap()
        );
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                assert!(!line.is_empty());
                headers.push_str(&line.to_lowercase());
            }
            assert!(headers.contains("authorization: bearer test-token"));
            assert!(headers.contains("chatgpt-account-id: test-account"));
            let body =
                r#"{"models":[{"slug":"gpt-future","visibility":"list","supported_in_api":true}]}"#;
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        });
        let credentials = auth::CodexCredentials {
            token: "test-token".into(),
            account: "test-account".into(),
        };
        assert_eq!(fetch(&endpoint, &credentials).unwrap(), ["gpt-future"]);
        server.join().unwrap();
    }
}
