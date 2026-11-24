#![allow(clippy::unwrap_used)]

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Clipboard {
        session: u32,
    },
    Agent {
        host: &'static str,
        service: &'static str,
    },
}

fn clipboard_prompt(who: &str) -> DecisionPrompt {
    DecisionPrompt::new(
        format!("{who} wants to copy 5 characters to the clipboard"),
        vec![
            DecisionChoice::primary("once", "Allow once"),
            DecisionChoice::secondary("always", "Always for this host"),
            DecisionChoice::secondary("deny", "Deny"),
        ],
    )
}

fn agent_prompt(host: &str, service: &str) -> DecisionPrompt {
    DecisionPrompt::new(
        format!("Cursor wants to check {service} on {host}"),
        vec![
            DecisionChoice::primary("allow_once", "Allow once"),
            DecisionChoice::secondary("allow_until_lock", "Allow until lock"),
            DecisionChoice::secondary("deny", "Deny"),
        ],
    )
    .with_detail("Agent", "Cursor (unverified)")
    .with_detail("Host", host)
    .with_command(format!(
        "systemctl show -p LoadState,ActiveState,SubState --value -- {service}"
    ))
}

#[test]
fn mixed_decisions_are_served_fifo_with_position_and_count() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let p1 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();
    let p3 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 2 }, clipboard_prompt("db-01")),
            t0,
        )
        .unwrap();

    let front = q.front(t0).unwrap();
    assert_eq!(front.id, p1.id());
    assert_eq!(front.kind, &Kind::Clipboard { session: 1 });
    assert_eq!(
        front.position,
        QueuePosition {
            current: 1,
            total: 3
        }
    );
    assert_eq!(front.position.label().as_deref(), Some("1 of 3"));

    let answered = q.answer(p1.id(), "once", t0);
    assert_eq!(answered, Some(Kind::Clipboard { session: 1 }));
    assert_eq!(
        p1.try_outcome(),
        Some(DecisionOutcome::Chosen("once".into()))
    );
    assert_eq!(p2.try_outcome(), None);

    let front = q.front(t0).unwrap();
    assert_eq!(front.id, p2.id());
    assert_eq!(
        front.kind,
        &Kind::Agent {
            host: "staging",
            service: "api.service"
        }
    );
    assert_eq!(
        front.position,
        QueuePosition {
            current: 1,
            total: 2
        }
    );
    assert_eq!(front.position.label().as_deref(), Some("1 of 2"));

    let answered = q.answer(p2.id(), "allow_until_lock", t0);
    assert_eq!(
        answered,
        Some(Kind::Agent {
            host: "staging",
            service: "api.service"
        })
    );
    assert_eq!(
        p2.try_outcome(),
        Some(DecisionOutcome::Chosen("allow_until_lock".into()))
    );

    let front = q.front(t0).unwrap();
    assert_eq!(front.id, p3.id());
    assert_eq!(
        front.position,
        QueuePosition {
            current: 1,
            total: 1
        }
    );
    assert_eq!(front.position.label(), None);

    let answered = q.answer(p3.id(), "deny", t0);
    assert_eq!(answered, Some(Kind::Clipboard { session: 2 }));
    assert_eq!(
        p3.try_outcome(),
        Some(DecisionOutcome::Chosen("deny".into()))
    );
    assert!(q.front(t0).is_none());
}

#[test]
fn a_decision_with_a_deadline_counts_down_and_expires_cleanly() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(60);

    let p1 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            )
            .with_deadline(deadline),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();

    assert_eq!(q.front(t0).unwrap().remaining_secs, Some(60));
    assert_eq!(q.next_wake(t0), Some(t0 + Duration::from_secs(1)));

    let t_mid = t0 + Duration::from_millis(600);
    assert_eq!(q.front(t_mid).unwrap().remaining_secs, Some(60));
    assert_eq!(q.next_wake(t_mid), Some(t0 + Duration::from_secs(1)));

    let t_one = t0 + Duration::from_secs(1);
    assert_eq!(q.front(t_one).unwrap().remaining_secs, Some(59));
    assert_eq!(q.next_wake(t_one), Some(t0 + Duration::from_secs(2)));

    let t_last = deadline - Duration::from_millis(200);
    assert_eq!(q.front(t_last).unwrap().remaining_secs, Some(1));
    assert_eq!(q.next_wake(t_last), Some(deadline));
    assert_eq!(p1.try_outcome(), None);

    // At the deadline, p1 expires and p2 advances to the front.
    let front = q.front(deadline).unwrap();
    assert_eq!(front.id, p2.id());
    assert_eq!(front.remaining_secs, None);
    assert_eq!(front.position.label(), None);
    assert_eq!(p1.try_outcome(), Some(DecisionOutcome::Expired));
    assert_eq!(q.next_wake(deadline), None);

    // Enqueueing with a deadline strictly in the past also expires immediately.
    let past = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            )
            .with_deadline(t0),
            deadline + Duration::from_secs(5),
        )
        .unwrap();
    assert_eq!(past.try_outcome(), Some(DecisionOutcome::Expired));
}

#[test]
fn a_queued_decision_behind_a_non_expiring_front_expires_on_time() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(30);

    let p1 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "prod",
                    service: "nginx.service",
                },
                agent_prompt("prod", "nginx.service"),
            )
            .with_deadline(deadline),
            t0,
        )
        .unwrap();

    assert_eq!(
        q.front(t0).unwrap().position.label().as_deref(),
        Some("1 of 2")
    );
    // Front has no deadline, so wake directly at p2's deadline without ticking every second.
    assert_eq!(q.next_wake(t0), Some(deadline));

    assert_eq!(q.sweep(deadline), 1);
    assert_eq!(p2.try_outcome(), Some(DecisionOutcome::Expired));
    assert_eq!(p1.try_outcome(), None);

    let front = q.front(deadline).unwrap();
    assert_eq!(front.id, p1.id());
    assert_eq!(front.position.total, 1);
    assert_eq!(front.position.label(), None);
    assert_eq!(q.next_wake(deadline), None);
}

#[test]
fn an_already_expired_request_expires_immediately_without_entering_the_queue() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let pending = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            )
            .with_deadline(t0),
            t0,
        )
        .unwrap();

    assert_eq!(pending.try_outcome(), Some(DecisionOutcome::Expired));
    assert!(q.front(t0).is_none());
}

#[test]
fn next_wake_picks_earlier_queued_deadline_over_front_countdown_tick() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();
    let front_deadline = t0 + Duration::from_secs(60);
    let earlier_queued = t0 + Duration::from_millis(250);

    let _p1 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "a",
                    service: "a.service",
                },
                agent_prompt("a", "a.service"),
            )
            .with_deadline(front_deadline),
            t0,
        )
        .unwrap();
    let _p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "b",
                    service: "b.service",
                },
                agent_prompt("b", "b.service"),
            )
            .with_deadline(earlier_queued),
            t0,
        )
        .unwrap();

    assert_eq!(q.next_wake(t0), Some(earlier_queued));
}

#[test]
fn external_cancel_handle_dismisses_front_decision_and_wakes_queue() {
    let mut q = DecisionQueue::new();
    let wake = q.cancel_wake();
    let t0 = Instant::now();

    let p1 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();

    let (_, waiter1, cancel1) = p1.into_parts();
    assert!(!cancel1.is_resolved());
    assert!(cancel1.cancel());
    assert!(cancel1.is_resolved());
    assert_eq!(waiter1.try_recv(), Some(DecisionOutcome::Cancelled));
    assert!(wake.try_recv().is_ok(), "external cancel must signal wake");
    assert_eq!(q.len(), 1);

    let front = q.front(t0).unwrap();
    assert_eq!(front.id, p2.id());
    assert_eq!(front.position.total, 1);
    assert_eq!(front.position.label(), None);
}

#[test]
fn cancelling_a_queued_decision_shrinks_count_without_disturbing_front() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let p1 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();
    let p3 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "prod",
                    service: "worker.service",
                },
                agent_prompt("prod", "worker.service"),
            ),
            t0,
        )
        .unwrap();

    assert_eq!(
        q.front(t0).unwrap().position.label().as_deref(),
        Some("1 of 3")
    );

    // Cancel p2 via external handle while queued behind p1.
    assert!(p2.cancel());
    assert_eq!(p2.try_outcome(), Some(DecisionOutcome::Cancelled));
    assert_eq!(q.front(t0).unwrap().id, p1.id());
    assert_eq!(
        q.front(t0).unwrap().position.label().as_deref(),
        Some("1 of 2")
    );

    // Cancel p3 via cancel_where while queued behind p1.
    assert_eq!(
        q.cancel_where(|k| matches!(k, Kind::Agent { host: "prod", .. })),
        1
    );
    assert_eq!(p3.try_outcome(), Some(DecisionOutcome::Cancelled));
    assert_eq!(q.front(t0).unwrap().id, p1.id());
    assert_eq!(q.front(t0).unwrap().position.label(), None);
    assert_eq!(p1.try_outcome(), None);
}

#[test]
fn cancel_where_dismisses_only_matching_kinds() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let clip = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let agent1 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();
    let agent2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "prod",
                    service: "db.service",
                },
                agent_prompt("prod", "db.service"),
            ),
            t0,
        )
        .unwrap();

    assert!(q.has_matching(|k| matches!(k, Kind::Clipboard { session: 1 })));
    assert!(agent1.cancel());
    // Already-cancelled agent1 is not counted again by cancel_where.
    let cancelled = q.cancel_where(|k| matches!(k, Kind::Agent { .. }));
    assert_eq!(cancelled, 1);
    assert_eq!(agent1.try_outcome(), Some(DecisionOutcome::Cancelled));
    assert_eq!(agent1.try_outcome(), None);
    assert_eq!(agent2.try_outcome(), Some(DecisionOutcome::Cancelled));
    assert_eq!(clip.try_outcome(), None);
    assert!(!q.has_matching(|k| matches!(k, Kind::Agent { .. })));
    assert_eq!(q.len(), 1);
    assert_eq!(q.front(t0).unwrap().id, clip.id());
}

#[test]
fn dropping_the_queue_cancels_remaining_pending_decisions() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let p1 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();

    drop(q);
    assert_eq!(p1.try_outcome(), Some(DecisionOutcome::Cancelled));
    assert_eq!(p2.try_outcome(), Some(DecisionOutcome::Cancelled));
}

#[test]
fn single_resolution_ignores_duplicate_answers_and_late_cancels() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let p1 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();

    let cancel1 = p1.cancel_handle();
    let cancel1_clone = p1.cancel_handle();

    // First answer resolves p1 once.
    assert_eq!(
        q.answer(p1.id(), "once", t0),
        Some(Kind::Clipboard { session: 1 })
    );
    assert_eq!(
        p1.try_outcome(),
        Some(DecisionOutcome::Chosen("once".into()))
    );

    // Late duplicate click for p1's ID must be ignored and must NOT answer p2.
    assert_eq!(q.answer(p1.id(), "deny", t0), None);
    assert_eq!(p1.try_outcome(), None);
    assert_eq!(p2.try_outcome(), None);
    assert_eq!(q.front(t0).unwrap().id, p2.id());

    // Late cancels after answer must be ignored and deliver no second outcome.
    assert!(!cancel1.cancel());
    assert!(!cancel1_clone.cancel());
    assert_eq!(p1.try_outcome(), None);
}

#[test]
fn late_click_after_external_cancel_is_ignored_and_preserves_next_decision() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let p1 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "prod",
                    service: "nginx.service",
                },
                agent_prompt("prod", "nginx.service"),
            ),
            t0,
        )
        .unwrap();

    // External caller cancels p1 before the UI has repainted or swept the queue.
    let c1 = p1.cancel_handle();
    assert!(c1.cancel());
    assert!(!c1.cancel(), "duplicate external cancel returns false");

    // User's click lands for p1's stale ID: ignored, p2 remains untouched.
    assert_eq!(q.answer(p1.id(), "allow_once", t0), None);
    assert_eq!(p1.try_outcome(), Some(DecisionOutcome::Cancelled));
    assert_eq!(p1.try_outcome(), None, "outcome delivered at most once");
    assert_eq!(p2.try_outcome(), None);
    assert_eq!(q.front(t0).unwrap().id, p2.id());
}

#[test]
fn late_click_at_or_after_expiry_delivers_expired_once_and_ignores_click() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(60);

    let p1 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            )
            .with_deadline(deadline),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();

    // Click lands right at deadline before timer tick swept the queue.
    assert_eq!(q.answer(p1.id(), "allow_once", deadline), None);
    assert_eq!(p1.try_outcome(), Some(DecisionOutcome::Expired));
    assert_eq!(p1.try_outcome(), None, "expiry delivered at most once");
    assert!(!p1.cancel(), "cancel after expiry is ignored");
    assert_eq!(p1.try_outcome(), None);

    // p2 is still waiting intact at the front.
    assert_eq!(p2.try_outcome(), None);
    assert_eq!(q.front(deadline).unwrap().id, p2.id());
}

#[test]
fn invalid_choice_or_non_front_id_is_rejected_without_advancing_queue() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();

    let p1 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();

    // Non-front ID cannot be answered out of order.
    assert_eq!(q.answer(p2.id(), "allow_once", t0), None);
    // Unknown choice ID on the front Decision is rejected.
    assert_eq!(q.answer(p1.id(), "allow_until_lock", t0), None);

    assert_eq!(p1.try_outcome(), None);
    assert_eq!(p2.try_outcome(), None);
    assert_eq!(q.len(), 2);
    assert_eq!(q.front(t0).unwrap().id, p1.id());
}

#[test]
fn queue_enforces_pending_limit_and_recovers_when_entries_leave() {
    let mut q = DecisionQueue::with_limit(2);
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(10);

    let p1 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p2 = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            )
            .with_deadline(deadline),
            t0,
        )
        .unwrap();

    let full = q.enqueue(
        DecisionRequest::new(Kind::Clipboard { session: 2 }, clipboard_prompt("db-01")),
        t0,
    );
    assert_eq!(full.unwrap_err(), QueueFull);

    // Once p2 expires, enqueue sweeps and accepts the new Decision.
    let p3 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 2 }, clipboard_prompt("db-01")),
            deadline,
        )
        .unwrap();
    assert_eq!(p2.try_outcome(), Some(DecisionOutcome::Expired));
    assert_eq!(q.len(), 2);
    assert_eq!(q.front(deadline).unwrap().id, p1.id());

    // External cancellation also frees capacity immediately on the next enqueue.
    assert!(p3.cancel());
    let p4 = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 3 }, clipboard_prompt("app-01")),
            deadline,
        )
        .unwrap();
    assert_eq!(q.len(), 2);
    assert_eq!(
        q.answer(p1.id(), "once", deadline),
        Some(Kind::Clipboard { session: 1 })
    );
    assert_eq!(q.front(deadline).unwrap().id, p4.id());
}

#[tokio::test]
async fn async_waiter_receives_answer_cancel_and_expiry_outcomes() {
    let mut q = DecisionQueue::new();
    let t0 = Instant::now();
    let deadline = t0 + Duration::from_secs(60);

    let p_ans = q
        .enqueue(
            DecisionRequest::new(Kind::Clipboard { session: 1 }, clipboard_prompt("web-01")),
            t0,
        )
        .unwrap();
    let p_can = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "staging",
                    service: "api.service",
                },
                agent_prompt("staging", "api.service"),
            ),
            t0,
        )
        .unwrap();
    let p_exp = q
        .enqueue(
            DecisionRequest::new(
                Kind::Agent {
                    host: "prod",
                    service: "db.service",
                },
                agent_prompt("prod", "db.service"),
            )
            .with_deadline(deadline),
            t0,
        )
        .unwrap();

    let (_, w_ans, _) = p_ans.into_parts();
    let (_, w_can, c_can) = p_can.into_parts();
    let (_, w_exp, _) = p_exp.into_parts();

    let front_id = q.front(t0).unwrap().id;
    assert!(q.answer(front_id, "always", t0).is_some());
    assert!(c_can.cancel());
    assert_eq!(q.sweep(deadline), 2);

    assert_eq!(w_ans.wait().await, DecisionOutcome::Chosen("always".into()));
    assert_eq!(w_can.wait().await, DecisionOutcome::Cancelled);
    assert_eq!(w_exp.wait().await, DecisionOutcome::Expired);
}

#[test]
fn prompt_strips_control_characters_ansi_escapes_and_bidi_overrides() {
    let prompt = DecisionPrompt::new(
        "Cursor\x1b[31m\x07 wants\r\n to check\u{202e} api.service",
        vec![
            DecisionChoice::primary("allow_once", "Allow\x00 once"),
            DecisionChoice::secondary("deny", "\x1b]52;c;aGk=\x07Deny"),
        ],
    )
    .with_detail("Agent\t", "bad\x1b[1m-agent\x7f")
    .with_command("systemctl\x07 show -- api.service\r\n\x1b[2K");

    assert_eq!(prompt.question, "Cursor wants to check api.service");
    assert_eq!(prompt.choices[0].id, "allow_once");
    assert_eq!(prompt.choices[0].label, "Allow once");
    assert_eq!(prompt.choices[1].label, "Deny");
    assert_eq!(prompt.details[0].label, "Agent");
    assert_eq!(prompt.details[0].value, "bad-agent");
    assert_eq!(
        prompt.command.as_deref(),
        Some("systemctl show -- api.service")
    );
    // Clean printable ASCII and Unicode (such as arrows) pass through unchanged.
    assert_eq!(
        sanitize_display_text("web-01 → staging (ops@10.0.0.5)"),
        "web-01 → staging (ops@10.0.0.5)"
    );
}

#[test]
fn concurrent_answer_and_external_cancel_resolve_at_most_once() {
    use std::sync::Barrier;

    for _ in 0..100 {
        let mut q = DecisionQueue::new();
        let t0 = Instant::now();
        let pending = q
            .enqueue(
                DecisionRequest::new(
                    Kind::Agent {
                        host: "staging",
                        service: "api.service",
                    },
                    agent_prompt("staging", "api.service"),
                ),
                t0,
            )
            .unwrap();
        let (id, waiter, cancel) = pending.into_parts();
        let barrier = Arc::new(Barrier::new(2));
        let b2 = Arc::clone(&barrier);

        let canceller = std::thread::spawn(move || {
            b2.wait();
            cancel.cancel()
        });

        barrier.wait();
        let answer_won = q.answer(id, "allow_once", t0).is_some();
        let cancel_won = canceller.join().unwrap();

        assert_ne!(
            answer_won, cancel_won,
            "exactly one of answer or cancel must win the race"
        );
        let outcome = waiter.try_recv().unwrap();
        if answer_won {
            assert_eq!(outcome, DecisionOutcome::Chosen("allow_once".into()));
        } else {
            assert_eq!(outcome, DecisionOutcome::Cancelled);
        }
        assert_eq!(
            waiter.try_recv(),
            None,
            "outcome must be delivered at most once"
        );
        assert!(q.front(t0).is_none());
    }
}

#[test]
fn external_cancel_wakes_gpui_foreground_task_with_no_window() {
    use gpui::AppContext as _;

    let mut app = gpui::TestApp::new();
    let (queue, pending) = app.update(|cx| {
        let mut q = DecisionQueue::new();
        let p = q
            .enqueue(
                DecisionRequest::new(
                    Kind::Agent {
                        host: "staging",
                        service: "api.service",
                    },
                    agent_prompt("staging", "api.service"),
                ),
                Instant::now(),
            )
            .unwrap();
        let wake = q.cancel_wake();
        let entity = cx.new(|_| q);
        let weak = entity.downgrade();
        cx.spawn(async move |cx| {
            if wake.recv().await.is_ok() {
                let _ = weak.update(cx, |q, _| {
                    q.sweep(Instant::now());
                });
            }
        })
        .detach();
        (entity, p)
    });

    let (_, waiter, cancel) = pending.into_parts();
    assert!(cancel.cancel());
    app.run_until_parked();

    assert_eq!(waiter.try_recv(), Some(DecisionOutcome::Cancelled));
    assert_eq!(waiter.try_recv(), None);
    let empty = app.update(|cx| queue.read(cx).is_empty());
    assert!(empty);
}
