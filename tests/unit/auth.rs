use super::*;
use serde_json::json;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "agent-auth-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn write(&self, value: &Value) -> std::path::PathBuf {
        let path = self.0.join("auth.json");
        fs::write(&path, value.to_string()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn valid() -> Value {
    json!({"version":1,"access_token":"test-token-never-log","account_id":"test-account",
        "expires_at_ms": (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() + 600_000) as u64})
}

#[test]
fn private_valid_profile_is_read_without_modification() {
    let f = Fixture::new();
    let path = f.write(&valid());
    let before = fs::read(&path).unwrap();
    assert!(from_file(&path).is_ok());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn rejects_expired_near_expired_wrong_version_and_missing_fields() {
    let f = Fixture::new();
    for (field, value) in [
        ("expires_at_ms", json!(0)),
        (
            "expires_at_ms",
            json!(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 30_000
            ),
        ),
        ("version", json!(2)),
        ("access_token", Value::Null),
        ("account_id", Value::Null),
        ("account_id", json!("bad\r\naccount")),
    ] {
        let mut data = valid();
        data[field] = value;
        let error = from_file(&f.write(&data))
            .err()
            .expect("unsafe profile accepted")
            .to_string();
        assert!(!error.contains("test-token-never-log"));
    }
}

#[test]
fn rejects_public_permissions_and_symlink() {
    let f = Fixture::new();
    let path = f.write(&valid());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(from_file(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = f.0.join("link");
    symlink(path, &link).unwrap();
    assert!(from_file(&link).is_err());
}

#[test]
fn rejects_nonregular_and_oversized_profiles() {
    let f = Fixture::new();
    assert!(from_file(&f.0).is_err());
    let path = f.write(&valid());
    fs::write(&path, vec![b' '; 1024 * 1024 + 1]).unwrap();
    assert!(from_file(&path).is_err());
    let fifo = f.0.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: name is a valid NUL-terminated path in the owned temporary fixture.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    assert!(from_file(&fifo).is_err()); // O_NONBLOCK must prevent a FIFO-open hang.
}
