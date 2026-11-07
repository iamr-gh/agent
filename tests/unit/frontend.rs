//! Engine tests use a nonterminal adapter, not the terminal frontend.
use super::*;
use crate::engine::Frontend;
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Clone, Default)]
pub(super) struct Recorder {
    events: Rc<RefCell<Vec<Value>>>,
    allow: bool,
}
impl Frontend for Recorder {
    fn approve(&self, name: &str, args: &Value) -> Result<()> {
        self.events
            .borrow_mut()
            .push(json!({"approval":name,"args":args}));
        if self.allow {
            Ok(())
        } else {
            Err("denied by test frontend".into())
        }
    }
    fn tool_started(&self, number: usize, name: &str, args: &Value) {
        self.events
            .borrow_mut()
            .push(json!({"started":name,"number":number,"args":args}));
    }
    fn tool_finished(&self, number: usize, name: &str, output: &str, is_error: bool, _: Duration) {
        self.events
            .borrow_mut()
            .push(json!({"finished":name,"number":number,"output":output,"is_error":is_error}));
    }
    fn diagnostic(&self, message: &str) {
        self.events.borrow_mut().push(json!({"diagnostic":message}));
    }
}

#[test]
fn engine_delivers_raw_events_and_preserves_raw_transcript() {
    let mut fixture = Fixture::new();
    let recorder = Recorder {
        allow: true,
        ..Default::default()
    };
    fixture.0.frontend = Box::new(recorder.clone());
    fixture.0.yes = false;
    let raw = "# Heading\n**bold**\u{1b}[31m";
    fs::write(fixture.0.root.join("a"), raw).unwrap();
    let mut req = request();
    let mut round = 0;
    turn(&mut req, &fixture.0, 3, 4 * 1024 * 1024, |_| {
        round += 1;
        Ok(if round == 1 {
            response(
                vec![
                    call("r", "read_file", json!({"path":"a"})),
                    call("w", "write_file", json!({"path":"b","content":raw})),
                ],
                "tool_calls",
            )
        } else {
            response(vec![], "stop")
        })
    })
    .unwrap();
    let events = recorder.events.borrow();
    assert_eq!(events.len(), 5);
    assert_eq!(events[0]["started"], "read_file");
    assert_eq!(
        events[1]["output"],
        format!("1\t# Heading\n2\t**bold**\u{1b}[31m\n")
    );
    assert_eq!(req.messages[2].content[0]["output"], events[1]["output"]);
    assert_eq!(events[2]["started"], "write_file");
    assert_eq!(events[3]["approval"], "write_file");
    assert_eq!(events[3]["args"]["content"], raw);
    assert_eq!(events[4]["is_error"], false);
    assert_eq!(fs::read_to_string(fixture.0.root.join("b")).unwrap(), raw);
}

#[test]
fn frontend_denial_settles_tool_without_writing() {
    let mut fixture = Fixture::new();
    let recorder = Recorder::default();
    fixture.0.frontend = Box::new(recorder.clone());
    fixture.0.yes = false;
    let mut req = request();
    let mut round = 0;
    turn(&mut req, &fixture.0, 2, 4 * 1024 * 1024, |_| {
        round += 1;
        Ok(if round == 1 {
            response(
                vec![call("w", "write_file", json!({"path":"a","content":"no"}))],
                "tool_calls",
            )
        } else {
            response(vec![], "stop")
        })
    })
    .unwrap();
    assert!(!fixture.0.root.join("a").exists());
    assert_eq!(req.messages[2].content[0]["is_error"], true);
    assert_eq!(recorder.events.borrow()[2]["is_error"], true);
}
