use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct Fixture(pub(crate) Workspace);
impl Fixture {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "agent-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(Workspace {
            root: root.canonicalize().unwrap(),
            yes: true,
            verbose: false,
        })
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0.root).unwrap();
    }
}
fn request() -> Request {
    Request {
        model: "test".into(),
        messages: vec![text("user", "task".into())],
        system: String::new(),
        tools: tools(),
        json_schema: None,
        max_tokens: 100,
        options: Default::default(),
    }
}
fn response(content: Vec<Value>, finish: &str) -> Response {
    Response {
        message: Message {
            role: "assistant".into(),
            content,
            replay: Some(json!({"opaque":"keep"})),
        },
        finish_reason: finish.into(),
        usage: json!({}),
    }
}
fn call(id: &str, name: &str, arguments: Value) -> Value {
    json!({"type":"tool_call","id":id,"name":name,"arguments":arguments})
}

#[test]
fn file_tools_validate_and_preserve_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let ws = &fixture.0;
    ws.execute("write_file", &json!({"path":"a","content":"one\ntwo\n"}))
        .unwrap();
    fs::set_permissions(ws.root.join("a"), fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(
        ws.execute(
            "read_file",
            &json!({"path":"a","start_line":2,"line_count":1})
        )
        .unwrap(),
        "2\ttwo\n"
    );
    ws.execute(
        "edit_file",
        &json!({"path":"a","old_string":"two","new_string":"three"}),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(ws.root.join("a")).unwrap(),
        "one\nthree\n"
    );
    assert_eq!(
        fs::metadata(ws.root.join("a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o640
    );
    for args in [
        json!({"path":"a","start_line":0}),
        json!({"path":"a","extra":1}),
        json!({"path":false}),
    ] {
        assert!(ws.execute("read_file", &args).is_err());
    }
    for old in ["", "missing", "e"] {
        assert!(
            ws.execute(
                "edit_file",
                &json!({"path":"a","old_string":old,"new_string":"x"})
            )
            .is_err()
        );
    }
    assert!(ws.execute("surprise", &json!({})).is_err());
}

#[test]
fn atomic_replacement_does_not_modify_hardlinked_alias() {
    let fixture = Fixture::new();
    let path = fixture.0.root.join("a");
    fs::write(&path, "aaa").unwrap();
    fs::hard_link(&path, fixture.0.root.join("alias")).unwrap();
    assert!(
        fixture
            .0
            .execute(
                "edit_file",
                &json!({"path":"a","old_string":"aa","new_string":"b"})
            )
            .is_err()
    );
    fixture
        .0
        .execute("write_file", &json!({"path":"a","content":"new"}))
        .unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.root.join("alias")).unwrap(),
        "aaa"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), "new");
    fs::create_dir(fixture.0.root.join("dir")).unwrap();
    assert!(atomic_write(&fixture.0.root.join("dir"), "fail").is_err());
    assert!(!fs::read_dir(&fixture.0.root).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".agent-")
    }));
}

#[test]
fn detached_writer_helper() {
    if std::env::var("AGENT_DETACH_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    // SAFETY: detach only this separately spawned test process; no shared Rust state.
    assert!(unsafe { libc::setsid() } >= 0);
    println!("DETACHED_PID:{}", std::process::id());
    io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(3));
}

#[test]
fn detached_pipe_writer_cannot_extend_shell_deadline() {
    let fixture = Fixture::new();
    let exe = std::env::current_exe()
        .unwrap()
        .to_string_lossy()
        .replace('\'', "'\\''");
    let command = format!(
        "AGENT_DETACH_FIXTURE=1 '{exe}' --exact tests::detached_writer_helper --nocapture & wait"
    );
    let start = Instant::now();
    let output = shell(&fixture.0.root, &command, Duration::from_millis(200)).unwrap();
    if let Some(pid) = output
        .split("DETACHED_PID:")
        .nth(1)
        .and_then(|s| s.lines().next())
        .and_then(|s| s.parse::<i32>().ok())
    {
        // SAFETY: PID was reported by the detached fixture we just started.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    } else {
        panic!("detached fixture did not start: {output}");
    }
    assert!(start.elapsed() < Duration::from_secs(1));
    assert!(output.contains("timeout"));
}

#[test]
fn file_paths_reject_parent_and_symlink_escapes() {
    let fixture = Fixture::new();
    assert!(fixture.0.path("../escape").is_err());
    std::os::unix::fs::symlink("/etc", fixture.0.root.join("outside")).unwrap();
    assert!(fixture.0.path("outside/passwd").is_err());
    assert!(fixture.0.path("outside/new-file").is_err());
    std::os::unix::fs::symlink("/nonexistent-agent-test", fixture.0.root.join("dangling")).unwrap();
    assert!(fixture.0.path("dangling").is_err());
}

#[test]
fn replay_and_tool_results_round_trip_in_order() {
    let fixture = Fixture::new();
    let mut req = request();
    let mut n = 0;
    turn(&mut req, &fixture.0, 3, 4 * 1024 * 1024, |req| {
        n += 1;
        if n == 1 {
            return Ok(response(
                vec![
                    call("a", "write_file", json!({"path":"a","content":"hello"})),
                    call("b", "bad_tool", json!({})),
                ],
                "tool_calls",
            ));
        }
        assert_eq!(req.messages[1].replay, Some(json!({"opaque":"keep"})));
        assert_eq!(req.messages[2].role, "user");
        assert_eq!(req.messages[2].content[0]["id"], "a");
        assert_eq!(req.messages[2].content[0]["is_error"], false);
        assert_eq!(req.messages[2].content[1]["id"], "b");
        assert_eq!(req.messages[2].content[1]["is_error"], true);
        Ok(response(vec![], "stop"))
    })
    .unwrap();
    assert_eq!(n, 2);
    assert_eq!(
        fs::read_to_string(fixture.0.root.join("a")).unwrap(),
        "hello"
    );
}

#[test]
fn incomplete_or_duplicate_calls_never_execute() {
    let fixture = Fixture::new();
    let c = call("a", "write_file", json!({"path":"a","content":"bad"}));
    for (calls, finish) in [
        (vec![c.clone()], "length"),
        (vec![c.clone()], "stop"),
        (vec![c.clone(), c], "tool_calls"),
        (vec![], "tool_calls"),
    ] {
        assert!(
            turn(&mut request(), &fixture.0, 2, 4 * 1024 * 1024, |_| Ok(
                response(calls.clone(), finish)
            ))
            .is_err()
        );
        assert!(!fixture.0.root.join("a").exists());
    }
}

#[test]
fn budgets_and_provider_errors_stop_without_retry() {
    let fixture = Fixture::new();
    let mut n = 0;
    assert!(
        turn(&mut request(), &fixture.0, 1, 4 * 1024 * 1024, |_| {
            n += 1;
            Ok(response(vec![call("a", "bad", json!({}))], "tool_calls"))
        })
        .unwrap_err()
        .to_string()
        .contains("turn limit")
    );
    assert_eq!(n, 1);
    let mut req = request();
    req.system = "x".repeat(512 * 1024);
    let size = serde_json::to_vec(&req).unwrap().len();
    assert!(turn(&mut req, &fixture.0, 1, size - 1, |_| panic!("over budget")).is_err());
    turn(&mut req, &fixture.0, 1, size, |_| {
        Ok(response(vec![], "stop"))
    })
    .unwrap();
    turn(&mut req, &fixture.0, 1, 4 * 1024 * 1024, |_| {
        Ok(response(vec![], "stop"))
    })
    .unwrap();
    assert!(
        turn(&mut request(), &fixture.0, 1, 4 * 1024 * 1024, |_| Err(
            "network failed".into()
        ))
        .is_err()
    );
}

#[test]
fn shell_collects_both_streams_and_reports_exit() {
    let fixture = Fixture::new();
    let output = shell(
        &fixture.0.root,
        "printf out; printf err >&2; exit 7",
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(output.contains("exit status: 7"));
    assert!(output.contains("stdout:\nout"));
    assert!(output.contains("stderr:\nerr"));
    assert!(
        shell(&fixture.0.root, "sleep 10", Duration::from_millis(50))
            .unwrap()
            .contains("timeout")
    );
    let start = Instant::now();
    shell(&fixture.0.root, "sleep 10 &", Duration::from_secs(1)).unwrap();
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn output_caps_unicode_and_escapes_terminal_controls() {
    let fixture = Fixture::new();
    let output = shell(
        &fixture.0.root,
        "head -c 100000 /dev/zero",
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(output.contains("[truncated]"));
    assert!(output.len() < OUTPUT_LIMIT + 200);
    let value = "é".repeat(OUTPUT_LIMIT);
    assert!(limited(&value).contains("[truncated]"));
    assert_eq!(display("a\x1b[2J\r\x07"), "a\\u{1b}[2J\\r\\u{7}");
}

#[test]
fn real_client_loopback_tool_round_trip() {
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    let fixture = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        for round in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut length = None;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = Some(value.trim().parse::<usize>().unwrap());
                }
            }
            let mut bytes = vec![0; length.unwrap()];
            reader.read_exact(&mut bytes).unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            if round == 1 {
                let input = body["input"].as_array().unwrap();
                assert!(input.iter().any(|v| v["encrypted_content"] == "opaque"));
                assert!(input.iter().any(|v| v["type"] == "function_call_output"
                    && v["call_id"] == "c1"
                    && v["output"] == "written"));
            }
            let output = if round == 0 {
                json!([
                    {"type":"reasoning","id":"r1","encrypted_content":"opaque","summary":[]},
                    {"type":"function_call","id":"f1","call_id":"c1","name":"write_file","arguments":"{\"path\":\"a\",\"content\":\"hello\"}"}
                ])
            } else {
                json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}])
            };
            let body = format!(
                "data: {}\n\n",
                json!({"type":"response.completed","response":{"output":output,"usage":{}}})
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        }
    });
    let client = Client::new(Provider::Openai, "test-only")
        .with_endpoint(endpoint)
        .unwrap();
    let mut req = request();
    turn(&mut req, &fixture.0, 3, 4 * 1024 * 1024, |r| {
        client.run(r, |_| true)
    })
    .unwrap();
    server.join().unwrap();
    assert_eq!(
        fs::read_to_string(fixture.0.root.join("a")).unwrap(),
        "hello"
    );
    assert_eq!(req.messages.last().unwrap().content[0]["text"], "done");
}

#[test]
fn noninteractive_approval_fails_closed() {
    if !io::stdin().is_terminal() {
        let mut fixture = Fixture::new();
        fixture.0.yes = false;
        assert!(
            fixture
                .0
                .execute("write_file", &json!({"path":"a","content":"no"}))
                .is_err()
        );
        assert!(
            fixture
                .0
                .execute("shell", &json!({"command":"touch a"}))
                .is_err()
        );
        assert!(!fixture.0.root.join("a").exists());
    }
}

#[path = "cancellation.rs"]
mod cancellation;
