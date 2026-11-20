use super::*;
use crate::engine::{Frontend, turn_saved};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[derive(Clone, Default)]
struct Guidance {
    pending: Rc<RefCell<Vec<String>>>,
    delivered: Rc<Cell<usize>>,
    after_tool: bool,
}
impl Frontend for Guidance {
    fn approve(&self, _: &str, _: &Value) -> Result<()> {
        Ok(())
    }
    fn tool_started(&self, _: usize, _: &str, _: &Value) {}
    fn tool_finished(&self, _: usize, _: &str, _: &str, _: bool, _: Duration) {
        if self.after_tool {
            self.pending.borrow_mut().push("change direction".into());
        }
    }
    fn diagnostic(&self, _: &str) {}
    fn steering(&self) -> Result<Vec<String>> {
        Ok(self.pending.borrow().clone())
    }
    fn steering_delivered(&self, count: usize) {
        self.pending.borrow_mut().drain(..count);
        self.delivered.set(self.delivered.get() + count);
    }
}

#[test]
fn guidance_skips_stale_calls_without_losing_results_or_native_replay() {
    for during_tool in [false, true] {
        let mut fixture = Fixture::new();
        let guidance = Guidance {
            after_tool: during_tool,
            ..Default::default()
        };
        fixture.0.frontend = Box::new(guidance.clone());
        let mut req = request();
        let mut round = 0;
        turn_saved(
            &mut req,
            &fixture.0,
            3,
            4 * 1024 * 1024,
            |req| {
                round += 1;
                if round == 1 {
                    if !during_tool {
                        guidance
                            .pending
                            .borrow_mut()
                            .push("change direction".into());
                    }
                    Ok(response(
                        vec![
                            call("a", "write_file", json!({"path":"first","content":"done"})),
                            call("b", "write_file", json!({"path":"stale","content":"bad"})),
                        ],
                        "tool_calls",
                    ))
                } else {
                    assert_eq!(req.messages[1].replay, Some(json!({"opaque":"keep"})));
                    assert_eq!(req.messages[2].content.len(), 2);
                    assert_eq!(req.messages[2].content[0]["id"], "a");
                    assert_eq!(req.messages[2].content[1]["id"], "b");
                    assert_eq!(req.messages[2].content[0]["is_error"], !during_tool);
                    assert!(
                        req.messages[2].content[1]["output"]
                            .as_str()
                            .unwrap()
                            .contains("superseded")
                    );
                    assert_eq!(req.messages[3].content[0]["text"], "change direction");
                    assert_eq!(guidance.delivered.get(), 1);
                    Ok(response(vec![], "stop"))
                }
            },
            |_| Ok(()),
        )
        .unwrap();
        assert_eq!(round, 2);
        assert_eq!(fixture.0.root.join("first").exists(), during_tool);
        assert!(!fixture.0.root.join("stale").exists());
    }
}

#[test]
fn guidance_arriving_during_final_answer_gets_another_request() {
    let mut fixture = Fixture::new();
    let guidance = Guidance::default();
    fixture.0.frontend = Box::new(guidance.clone());
    let mut req = request();
    let mut round = 0;
    turn_saved(
        &mut req,
        &fixture.0,
        3,
        4096,
        |req| {
            round += 1;
            if round == 1 {
                guidance.pending.borrow_mut().push("also this".into());
            } else {
                assert_eq!(req.messages.last().unwrap().content[0]["text"], "also this");
            }
            Ok(response(vec![], "stop"))
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(round, 2);
    assert_eq!(guidance.delivered.get(), 1);
}

#[test]
fn guidance_after_last_tool_is_checkpointed_even_at_the_turn_limit() {
    let mut fixture = Fixture::new();
    let guidance = Guidance {
        after_tool: true,
        ..Default::default()
    };
    fixture.0.frontend = Box::new(guidance.clone());
    let mut req = request();
    let error = turn_saved(
        &mut req,
        &fixture.0,
        1,
        4096,
        |_| {
            Ok(response(
                vec![call("a", "read_file", json!({"path":"missing"}))],
                "tool_calls",
            ))
        },
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(error.to_string().contains("turn limit"));
    assert_eq!(guidance.delivered.get(), 1);
    assert_eq!(
        req.messages.last().unwrap().content[0]["text"],
        "change direction"
    );
}

#[test]
fn guidance_is_not_acknowledged_on_checkpoint_failure_and_respects_context_cap() {
    for fail_save in [true, false] {
        let mut fixture = Fixture::new();
        let guidance = Guidance::default();
        guidance
            .pending
            .borrow_mut()
            .push("large guidance".repeat(100));
        fixture.0.frontend = Box::new(guidance.clone());
        let error = turn_saved(
            &mut request(),
            &fixture.0,
            2,
            100,
            |_| panic!("must not infer after failed save or context cap"),
            |_| {
                if fail_save {
                    Err("save failed".into())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        assert_eq!(guidance.delivered.get(), usize::from(!fail_save));
        assert!(error.to_string().contains(if fail_save {
            "save failed"
        } else {
            "context exceeds"
        }));
    }
}
