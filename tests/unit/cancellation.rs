//! Real chat/transport cancellation, isolated from the runner's SIGINT state.
use super::*;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, ExitStatus};

const HELPER: &str = "AGENT_CANCELLATION_HELPER";
const LIMIT: Duration = Duration::from_secs(8);

#[test]
fn process_helper() {
    if std::env::var(HELPER).as_deref() != Ok("1") {
        return;
    }
    install_interrupt_handler();
    let client = Client::new(Provider::Openai, "loopback-test-only")
        .with_endpoint(std::env::var("AGENT_CANCEL_ENDPOINT").unwrap())
        .unwrap();
    let workspace = Workspace {
        root: PathBuf::from(std::env::var_os("AGENT_CANCEL_WORKSPACE").unwrap()),
        yes: std::env::var("AGENT_CANCEL_APPROVAL").as_deref() != Ok("1"),
        frontend: Box::new(crate::ui::TerminalFrontend { verbose: false }),
    };
    let prompt = if std::env::var("AGENT_CANCEL_BATCH").as_deref() == Ok("1") {
        vec!["task".into()]
    } else {
        vec![]
    };
    let mut req = request();
    req.messages.clear();
    let result = chat(client, workspace, req, prompt, 5, 4 * 1024 * 1024);
    let cancelled = INTERRUPTED.load(Ordering::Relaxed)
        || result
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<llm_api::Cancelled>());
    if let Err(error) = &result {
        eprintln!("helper: {error}");
    }
    std::process::exit(if cancelled {
        130
    } else if result.is_err() {
        1
    } else {
        0
    });
}

struct Process {
    child: Child,
    terminal: Option<File>,
    output: String,
}

impl Process {
    fn spawn(fixture: &Fixture, endpoint: &str, interactive: bool, approval: bool) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::cancellation::process_helper",
                "--nocapture",
            ])
            .env(HELPER, "1")
            .env("AGENT_CANCEL_ENDPOINT", endpoint)
            .env("AGENT_CANCEL_WORKSPACE", &fixture.0.root)
            .env("AGENT_CANCEL_APPROVAL", if approval { "1" } else { "0" })
            .env("AGENT_CANCEL_BATCH", if interactive { "0" } else { "1" });
        let terminal = if interactive {
            let (mut master, mut slave): (RawFd, RawFd) = (-1, -1);
            let mut size = libc::winsize {
                ws_row: 24,
                ws_col: 40,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            // SAFETY: openpty initializes both descriptors and borrows size.
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut size,
                    )
                },
                0
            );
            // SAFETY: successful openpty transferred ownership of these unique descriptors.
            let master = unsafe { File::from_raw_fd(master) };
            let slave = unsafe { File::from_raw_fd(slave) };
            // The child must only inherit the slave, never its own PTY master.
            // SAFETY: master is live; preserve its existing descriptor flags.
            let fd_flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFD) };
            assert!(fd_flags >= 0);
            assert_eq!(
                unsafe {
                    libc::fcntl(
                        master.as_raw_fd(),
                        libc::F_SETFD,
                        fd_flags | libc::FD_CLOEXEC,
                    )
                },
                0
            );
            command
                .stdin(slave.try_clone().unwrap())
                .stdout(slave.try_clone().unwrap())
                .stderr(slave);
            // SAFETY: only async-signal-safe syscalls run between fork and exec.
            // The slave is already stdin; make it this new session's controlling
            // terminal and foreground the helper so terminal Ctrl-C delivers SIGINT.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1
                        || libc::ioctl(libc::STDIN_FILENO, libc::TIOCSCTTY as _, 0) == -1
                        || libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) == -1
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            // SAFETY: master is live; preserve existing descriptor flags.
            let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
            assert!(flags >= 0);
            assert_eq!(
                unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
                0
            );
            Some(master)
        } else {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            None
        };
        Self {
            child: command.spawn().unwrap(),
            terminal,
            output: String::new(),
        }
    }

    fn resize(&mut self, rows: u16, columns: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the owned PTY descriptor is live and ioctl borrows size.
        assert_eq!(
            unsafe {
                libc::ioctl(
                    self.terminal.as_ref().unwrap().as_raw_fd(),
                    libc::TIOCSWINSZ,
                    &size,
                )
            },
            0
        );
    }

    fn send(&mut self, input: &str) {
        self.terminal
            .as_mut()
            .unwrap()
            .write_all(input.replace('\n', "\r").as_bytes())
            .unwrap();
        // Emulate an emulator continuously draining redraw output, including
        // when a test types or erases thousands of characters before submitting.
        let deadline = Instant::now() + Duration::from_millis(100);
        while Instant::now() < deadline {
            let mut bytes = [0; 8192];
            match self.terminal.as_mut().unwrap().read(&mut bytes) {
                Ok(n) => self.output.push_str(&String::from_utf8_lossy(&bytes[..n])),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // A queued /quit can exit before this drain window ends.
                Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
                Err(e) => panic!("terminal read: {e}"),
            }
        }
    }

    fn interrupt(&self) {
        // SAFETY: this is the owned, unreaped subprocess, not the test runner.
        assert_eq!(
            unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) },
            0
        );
    }

    fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(end) = self.output.find(needle) {
                self.output.drain(..end + needle.len());
                return;
            }
            assert!(
                Instant::now() < deadline,
                "waiting for {needle:?}: {:?}",
                self.output
            );
            let mut bytes = [0; 4096];
            match self.terminal.as_mut().unwrap().read(&mut bytes) {
                Ok(0) => panic!("terminal closed waiting for {needle:?}: {:?}", self.output),
                Ok(n) => self.output.push_str(&String::from_utf8_lossy(&bytes[..n])),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("terminal read: {e}; output: {:?}", self.output),
            }
        }
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + LIMIT;
        loop {
            // A controlling-terminal session leader can wait for terminal output
            // to drain on exit (notably on macOS). Consume the final /quit echo.
            if let Some(terminal) = self.terminal.as_mut() {
                let mut bytes = [0; 4096];
                match terminal.read(&mut bytes) {
                    Ok(n) => self.output.push_str(&String::from_utf8_lossy(&bytes[..n])),
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) || e.raw_os_error() == Some(libc::EIO) => {}
                    Err(e) => panic!("terminal read while waiting for exit: {e}"),
                }
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "helper did not exit: {:?}",
                self.output
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn quit(&mut self) {
        self.send("/quit\n");
        assert_eq!(self.wait().code(), Some(0));
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        // Closing the master also releases a child stuck draining its terminal.
        drop(self.terminal.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Server(TcpListener);
impl Server {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self(listener)
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.0.local_addr().unwrap())
    }

    fn request(&self) -> (TcpStream, Value) {
        let deadline = Instant::now() + LIMIT;
        let mut stream = loop {
            match self.0.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "no loopback request");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("accept: {e}"),
            }
        };
        // BSD may inherit O_NONBLOCK from the listener on accepted sockets.
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(LIMIT)).unwrap();
        stream.set_write_timeout(Some(LIMIT)).unwrap();
        let mut reader = BufReader::new(&mut stream);
        let mut length = None;
        let mut header_bytes = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            header_bytes += line.len();
            assert!(header_bytes < 64 * 1024);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let length = length.expect("content-length");
        assert!(length < 1024 * 1024);
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        (stream, serde_json::from_slice(&body).unwrap())
    }
}

fn complete(mut stream: TcpStream, output: Value) {
    let body = format!(
        "data: {}\n\n",
        json!({
            "type":"response.completed", "response":{"output":output,"usage":{}}
        })
    );
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
}

fn done(stream: TcpStream) {
    complete(
        stream,
        json!([{"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":"done"}]}]),
    );
}

fn tool(stream: TcpStream, name: &str, args: Value) {
    complete(
        stream,
        json!([{"type":"function_call","id":"f1","call_id":"cancel-call",
        "name":name,"arguments":args.to_string()}]),
    );
}

fn stall(stream: &mut TcpStream) {
    // Valid SSE headers, but no events: cancellation must close the actual socket.
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n").unwrap();
}

fn assert_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("cancelled transport must close its peer: {other:?}"),
    }
}

#[test]
fn long_input_backspaces_across_viewports_and_submits_exact_text() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("keep this ");
    // Exceed both macOS and Linux canonical line limits, then erase everything
    // beyond the prefix, across many 40-column viewport boundaries.
    for _ in 0..6 {
        process.send(&"x".repeat(1000));
    }
    for _ in 0..6 {
        process.send(&"\x7f".repeat(1000));
    }
    process.send("界e\u{301}");
    process.send("\x7f\x7f"); // erase graphemes, not UTF-8 bytes
    process.send("finished\n");
    let (stream, body) = server.request();
    assert!(
        body["input"].as_array().unwrap().iter().any(|message| {
            message["role"] == "user"
                && message["content"].as_array().is_some_and(|content| {
                    content
                        .iter()
                        .any(|part| part["text"] == "keep this finished\n")
                })
        }),
        "unexpected submitted input: {body}"
    );
    done(stream);
    process.expect("> ");
    process.quit();
}

#[test]
fn cursor_editing_and_history_preserve_the_draft() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    // Insert before a wide grapheme, erase a combining grapheme, then delete.
    process.send("a界e\u{301}z\x1b[D\x7f\x1b[DX\x1b[C\x1b[3~\n");
    let (stream, body) = server.request();
    assert_user_text(&body, "aX界\n");
    done(stream);
    process.expect("> ");
    // Down restores the unfinished draft; repeated up/down is bounded.
    process.send("draft\x1b[A\x1b[A\x1b[B\x1b[B!\n");
    let (stream, body) = server.request();
    assert_user_text(&body, "draft!\n");
    done(stream);
    process.expect("> ");
    // Recall the older entry and edit it rather than only displaying history.
    process.send("\x1b[A\x1b[A\x1b[H\x1b[3~Q\x1b[F!\n");
    let (stream, body) = server.request();
    assert_user_text(&body, "QX界!\n");
    done(stream);
    process.expect("> ");
    process.quit();
}

fn assert_user_text(body: &Value, expected: &str) {
    let message = body["input"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .unwrap();
    assert!(
        message["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|part| part["text"] == expected),
        "unexpected input: {body}"
    );
}

#[test]
fn input_typed_during_response_survives_until_the_next_prompt() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("first\n");
    let (stream, _) = server.request();
    // Queue an unfinished line while the model request is still running.
    process.send("queued ac\x1b[Db");
    // The draft must be visible before the stalled request returns, not just
    // preserved invisibly as terminal typeahead.
    process.expect("queued abc");
    done(stream);
    process.expect("queued abc");
    process.send("\n");
    let (stream, body) = server.request();
    assert_user_text(&body, "queued abc\n");
    // A complete queued prompt must also survive the transition.
    process.send("third\n");
    done(stream);
    let (stream, body) = server.request();
    assert_user_text(&body, "third\n");
    done(stream);
    process.expect("> ");
    process.quit();
}

#[test]
fn live_queue_is_fifo_and_unfinished_draft_survives_busy_turns() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("first\n");
    let (stream, _) = server.request();
    process.send("second\nthird\n界e\u{301} ac\x1b[Db");
    process.expect("Queued: second");
    process.expect("Queued: third");
    process.expect("界e\u{301} abc");
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    done(stream);
    let (stream, body) = server.request();
    assert_user_text(&body, "second\n");
    done(stream);
    let (stream, body) = server.request();
    assert_user_text(&body, "third\n");
    done(stream);
    process.expect("界e\u{301} abc");
    // Cursor was between b and c, not at the end, throughout both queued turns.
    process.send("X\n");
    let (stream, body) = server.request();
    assert_user_text(&body, "界e\u{301} abXc\n");
    done(stream);
    process.expect("> ");
    process.quit();
}

#[test]
fn live_input_survives_narrow_and_short_terminal_resizes() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("first\n");
    let (stream, _) = server.request();
    process.send("live 界e\u{301}🙂");
    process.expect("live 界e\u{301}🙂");
    process.resize(8, 2);
    process.expect("\x1b[1;5r");
    process.resize(2, 2);
    process.expect("\x1b[r\x1b[2;1H");
    process.resize(12, 80);
    process.expect("live 界e\u{301}🙂");
    process.send("\n");
    done(stream);
    let (stream, body) = server.request();
    assert_user_text(&body, "live 界e\u{301}🙂\n");
    done(stream);
    process.expect("> ");
    process.quit();
}

#[test]
fn live_typeahead_cannot_answer_an_approval_and_draft_is_restored() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, true);
    process.expect("> ");
    process.send("first\n");
    let (stream, _) = server.request();
    process.send("y\nunfinished draft");
    process.expect("Queued: y");
    process.expect("unfinished draft");
    tool(
        stream,
        "write_file",
        json!({"path":"must-not-exist", "content":"bad"}),
    );
    process.expect("Allow with full user privileges? [y/N] ");
    assert!(!fixture.0.root.join("must-not-exist").exists());
    process.send("n\n");
    let (stream, _) = server.request();
    process.expect("unfinished draft");
    assert!(!fixture.0.root.join("must-not-exist").exists());
    done(stream);
    let (stream, body) = server.request();
    assert_user_text(&body, "y\n");
    done(stream);
    process.expect("unfinished draft");
    process.send("\x15/quit\n");
    assert_eq!(process.wait().code(), Some(0));
}

#[test]
fn tab_promotes_visible_pending_and_skips_tools_after_the_running_request() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("first\n");
    let (stream, _) = server.request();
    process.send("deferred\nnew direction\n\x1b[A\t");
    process.expect("Steering pending: new direction");
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    // Steering must neither cancel this request nor execute its now-stale tool.
    tool(
        stream,
        "write_file",
        json!({"path":"stale", "content":"bad"}),
    );
    let (stream, body) = server.request();
    assert_user_text(&body, "new direction\n");
    assert!(body.to_string().contains("superseded by user steering"));
    assert!(!fixture.0.root.join("stale").exists());
    process.expect("Delivered: new direction");
    done(stream);
    let (stream, body) = server.request();
    assert_user_text(&body, "deferred\n");
    done(stream);
    process.expect("> ");
    process.quit();
}

#[test]
fn draft_steering_waits_for_running_shell_and_skips_the_rest_of_its_batch() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("first\n");
    let (stream, _) = server.request();
    complete(
        stream,
        json!([
            {"type":"function_call","id":"f1","call_id":"running","name":"shell",
             "arguments":json!({"command":"printf started > started; while [ ! -f release ]; do sleep 0.01; done; printf completed > completed"}).to_string()},
            {"type":"function_call","id":"f2","call_id":"stale","name":"write_file",
             "arguments":json!({"path":"stale","content":"bad"}).to_string()}
        ]),
    );
    let deadline = Instant::now() + LIMIT;
    while !fixture.0.root.join("started").exists() {
        assert!(Instant::now() < deadline, "shell did not start");
        thread::sleep(Duration::from_millis(5));
    }
    process.send("change direction\t");
    process.expect("Steering pending: change direction");
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    fs::write(fixture.0.root.join("release"), "go").unwrap();
    let (stream, body) = server.request();
    assert!(fixture.0.root.join("completed").exists());
    assert!(!fixture.0.root.join("stale").exists());
    assert_user_text(&body, "change direction\n");
    assert!(body.to_string().contains("superseded by user steering"));
    done(stream);
    process.expect("> ");
    process.quit();
}

#[test]
fn escape_preserves_queue_and_draft_without_automatic_drain() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("first\n");
    let (mut stream, _) = server.request();
    // Unconsumed steering must also remain paused after Esc.
    process.send("first pending\nsecond pending\tdraft ac\x1b[Db");
    process.expect("draft abc");
    process.send("\x1b");
    process.expect("pending input preserved");
    assert_closed(&mut stream);
    process.expect("Paused");
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    // The draft cursor, not just its text, survived Esc. Send it explicitly.
    process.send("X\n");
    let (stream, body) = server.request();
    assert_user_text(&body, "draft abXc\n");
    done(stream);
    process.expect("Paused");
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    process.send("\x1b[A\t");
    let (stream, body) = server.request();
    assert_user_text(&body, "second pending\n");
    done(stream);
    process.expect("Paused");
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
    process.send("\x1b[A\x1b[3~");
    process.send("/quit\n");
    assert_eq!(process.wait().code(), Some(0));
}

#[test]
fn idle_prompt_interrupt_then_quit() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.interrupt();
    process.expect("cancelled");
    process.expect("> ");
    process.quit();
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn terminal_ctrl_c_flushes_partial_input_then_quit() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("must-not-be-submitted");
    // Wait for terminal echo so Ctrl-C cannot race ahead of the partial input.
    process.expect("must-not-be-submitted");
    process.send("\x03");
    process.expect("cancelled");
    process.expect("> ");
    // If the terminal failed to flush its canonical input, this newline would
    // submit the stale text and make a request instead of returning a prompt.
    process.send("\n");
    process.expect("> ");
    process.quit();
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn stalled_network_interrupt_then_successful_turn() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("first\n");
    let (mut stream, _) = server.request();
    stall(&mut stream);
    process.interrupt();
    assert_closed(&mut stream);
    process.expect("cancelled");
    process.expect("> ");
    process.send("second\n");
    let (stream, body) = server.request();
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.to_string().contains("second"))
    );
    done(stream);
    process.expect("> ");
    process.quit();
}

#[test]
fn approval_interrupt_prevents_mutation() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, true);
    process.expect("> ");
    process.send("write\n");
    let (stream, _) = server.request();
    tool(
        stream,
        "write_file",
        json!({"path":"must-not-exist","content":"bad"}),
    );
    process.expect("Allow with full user privileges? [y/N] ");
    process.interrupt();
    process.expect("cancelled; completed changes were not rolled back");
    process.expect("> ");
    process.quit();
    assert!(!fixture.0.root.join("must-not-exist").exists());
}

// Also clean up the tool's group if an assertion fails before chat cancels it.
struct ShellGroup(i32);
impl Drop for ShellGroup {
    fn drop(&mut self) {
        // SAFETY: PID is the dedicated shell group reported by our own fixture.
        unsafe {
            libc::kill(-self.0, libc::SIGKILL);
        }
    }
}

fn pid_file(path: &Path) -> i32 {
    let deadline = Instant::now() + LIMIT;
    loop {
        if let Ok(value) = fs::read_to_string(path) {
            if let Ok(pid) = value.parse::<i32>() {
                assert!(pid > 1);
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "missing PID file: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn shell_interrupt_cleans_group_and_preserves_tool_result() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), true, false);
    process.expect("> ");
    process.send("shell\n");
    let (stream, _) = server.request();
    let native = json!([
        {"type":"reasoning","id":"reasoning-cancel","encrypted_content":"opaque-cancellation-replay","summary":[]},
        {"type":"function_call","id":"effect-native","call_id":"effect-call","name":"shell",
         "arguments":json!({"command":"printf 'completed\\n' >> completed-effect; printf 'effect-complete'"}).to_string()},
        {"type":"function_call","id":"cancel-native","call_id":"cancel-call","name":"shell",
         "arguments":json!({"command":
            "test -f completed-effect || exit 1; printf '%s' \"$$\" > shell.pid; sleep 60 & printf '%s' \"$!\" > sleep.pid; wait"}).to_string()},
        {"type":"function_call","id":"pending-native","call_id":"pending-call","name":"write_file",
         "arguments":json!({"path":"must-not-exist","content":"bad"}).to_string()}
    ]);
    complete(stream, native.clone());
    let group = ShellGroup(pid_file(&fixture.0.root.join("shell.pid")));
    let sleeper = pid_file(&fixture.0.root.join("sleep.pid"));
    process.send("discard this queued message\nunfinished draft");
    process.expect("unfinished draft");
    process.interrupt();
    process.expect("cancelled; completed changes were not rolled back");
    process.expect("> ");
    let deadline = Instant::now() + LIMIT;
    for pid in [group.0, sleeper] {
        // SAFETY: signal zero only queries existence of our fixture processes.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "tool process {pid} survived cancellation"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    let effect = fixture.0.root.join("completed-effect");
    let pending = fixture.0.root.join("must-not-exist");
    assert_eq!(fs::read_to_string(&effect).unwrap(), "completed\n");
    assert!(!pending.exists(), "pending mutation ran after interruption");
    process.send("continue\n");
    let (stream, body) = server.request();
    assert_user_text(&body, "continue\n");
    let input = body["input"].as_array().unwrap();
    let replay: Vec<_> = input
        .iter()
        .filter(|v| v["type"] == "reasoning" || v["type"] == "function_call")
        .cloned()
        .collect();
    assert_eq!(
        replay,
        *native.as_array().unwrap(),
        "native reasoning and ordered calls must replay unchanged: {body}"
    );
    let results: Vec<_> = input
        .iter()
        .enumerate()
        .filter(|(_, v)| v["type"] == "function_call_output")
        .collect();
    assert_eq!(results.len(), 3, "exactly one result per call: {body}");
    for ((result_index, result), call) in results.iter().zip(&native.as_array().unwrap()[1..]) {
        assert_eq!(
            result["call_id"], call["call_id"],
            "ordered results: {body}"
        );
        let call_index = input.iter().position(|v| v == call).unwrap();
        assert!(
            call_index < *result_index,
            "result precedes its call: {body}"
        );
    }
    assert!(
        results[0].1["output"]
            .as_str()
            .unwrap()
            .contains("effect-complete")
    );
    for (_, result) in &results[1..] {
        assert!(
            result["output"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("cancel"),
            "interrupted and pending calls need cancellation results: {body}"
        );
    }
    done(stream);
    process.expect("> ");
    process.quit();
    assert_eq!(
        fs::read_to_string(effect).unwrap(),
        "completed\n",
        "completed effect must persist exactly once across continuation"
    );
    assert!(!pending.exists(), "continuation ran the pending mutation");
    assert!(matches!(server.0.accept(), Err(e) if e.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn batch_stalled_network_interrupt_exits_130() {
    let fixture = Fixture::new();
    let server = Server::new();
    let mut process = Process::spawn(&fixture, &server.endpoint(), false, false);
    let (mut stream, _) = server.request();
    stall(&mut stream);
    process.interrupt();
    assert_closed(&mut stream);
    assert_eq!(process.wait().code(), Some(130));
}
