//! Login/profile ownership belongs to the application, not llm-api's transport.
use llm_api::{Client, Result};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn codex_client() -> Result<Client> {
    use std::env::{VarError, var, var_os};
    match (var("CODEX_ACCESS_TOKEN"), var("CODEX_ACCOUNT_ID")) {
        (Ok(token), Ok(account)) => return Client::codex(token, account),
        (Err(VarError::NotPresent), Err(VarError::NotPresent)) => {}
        _ => return Err("set both CODEX_ACCESS_TOKEN and CODEX_ACCOUNT_ID, or neither".into()),
    }
    let home = var_os("HOME").ok_or("HOME is required to locate the Codex login")?;
    let own = var_os("AGENT_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join(".agent"))
        .join("codex-auth.json");
    match fs::symlink_metadata(&own) {
        Ok(_) => return from_file(&own),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let legacy = var_os("FX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(&home).join(".fx"))
        .join("chatgpt-auth.json");
    from_file(&legacy).map_err(|e| format!("Codex login unavailable: {e}. Set both CODEX environment variables or renew the existing fx login").into())
}

fn from_file(path: &Path) -> Result<Client> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    // SAFETY: getuid only reads the current process's real user ID.
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } || meta.mode() & 0o077 != 0 {
        return Err("Codex auth must be a private, user-owned regular file".into());
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err("Codex auth file exceeds 1 MiB".into());
    }
    let data: Value = serde_json::from_slice(&bytes)?;
    if data["version"] != 1 {
        return Err("unsupported Codex auth schema".into());
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    if u128::from(data["expires_at_ms"].as_u64().unwrap_or(0)) < now + 60_000 {
        return Err(
            "Codex access token expired or near expiry; refresh it through its login owner".into(),
        );
    }
    Client::codex(
        data["access_token"]
            .as_str()
            .ok_or("missing Codex access token")?,
        data["account_id"]
            .as_str()
            .ok_or("missing Codex account ID")?,
    )
}

#[cfg(test)]
#[path = "../tests/unit/auth.rs"]
mod tests;
