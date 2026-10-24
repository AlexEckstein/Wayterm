//! Display-free tests: the store roundtrip through drafts, the validation
//! the dialogs share, the status cells the rows render, and the fake
//! source that stands in for [`super::status_source::EngineStatusBridge`].

use super::model::{
    draft_from_rule, drafts_from_rules, kind_from_index, kind_index_for, lookup_status,
    rules_from_drafts, status_text, validate_draft, ForwardRuleDraft, KIND_INDEX_DYNAMIC,
    KIND_INDEX_LOCAL, KIND_INDEX_REMOTE, OFFLINE_STATUS, V2_NOTE,
};
use super::status_source::{
    EngineStatusBridge, FakeStatusSource, ForwardEvent, ForwardState, ForwardStatusSource,
};
use crate::store::model::{ForwardKind, ForwardRule};
use crate::transport::forward::{ForwardEngine, OpenerFuture};
use crate::transport::types::TransportError;

fn sample_rule() -> ForwardRule {
    ForwardRule {
        kind: ForwardKind::Local,
        bind_addr: "127.0.0.1".to_string(),
        bind_port: 8080,
        remote_host: "web1.lan".to_string(),
        remote_port: 80,
        enabled: true,
    }
}

fn sample_draft() -> ForwardRuleDraft {
    ForwardRuleDraft {
        kind_index: KIND_INDEX_LOCAL,
        bind_addr: "127.0.0.1".to_string(),
        bind_port: "8080".to_string(),
        remote_host: "web1.lan".to_string(),
        remote_port: "80".to_string(),
        enabled: true,
    }
}

#[test]
fn kind_index_round_trips() {
    for kind in [
        ForwardKind::Local,
        ForwardKind::Remote,
        ForwardKind::Dynamic,
    ] {
        assert_eq!(kind_from_index(kind_index_for(&kind)), kind);
    }
    assert_eq!(kind_index_for(&ForwardKind::Local), KIND_INDEX_LOCAL);
    assert_eq!(kind_index_for(&ForwardKind::Remote), KIND_INDEX_REMOTE);
    assert_eq!(kind_index_for(&ForwardKind::Dynamic), KIND_INDEX_DYNAMIC);
    assert_eq!(kind_from_index(99), ForwardKind::Local);
}

#[test]
fn rule_round_trips_through_draft() {
    // The store roundtrip stays intact: what the dialog edits is what the
    // row held, wire keys untouched.
    let rule = sample_rule();
    assert_eq!(validate_draft(&draft_from_rule(&rule)), Ok(rule));
}

#[test]
fn no_stored_rules_is_no_draft_rows() {
    assert!(drafts_from_rules(None).is_empty());
    assert!(drafts_from_rules(Some(&vec![])).is_empty());
}

#[test]
fn valid_drafts_convert() {
    let mut second = sample_draft();
    second.bind_port = "2222".to_string();
    second.remote_host = "db.lan".to_string();
    second.remote_port = "22".to_string();
    second.enabled = false;
    let rules = rules_from_drafts(&[sample_draft(), second]).expect("valid drafts");
    assert_eq!(rules.len(), 2);
    assert_eq!(rules[0], sample_rule());
    assert_eq!(rules[1].bind_port, 2222);
    assert!(!rules[1].enabled);
}

#[test]
fn non_numeric_ports_are_errors() {
    for (bind_port, remote_port) in [("http", "80"), ("8080", "http"), ("", "80"), ("0", "80")] {
        let mut draft = sample_draft();
        draft.bind_port = bind_port.to_string();
        draft.remote_port = remote_port.to_string();
        assert!(
            validate_draft(&draft).is_err(),
            "ports {bind_port}/{remote_port} must not convert"
        );
    }
}

#[test]
fn empty_bind_and_remote_are_errors() {
    let mut draft = sample_draft();
    draft.bind_addr = "   ".to_string();
    assert!(
        validate_draft(&draft).is_err(),
        "empty bind must not convert"
    );
    let mut draft = sample_draft();
    draft.remote_host = String::new();
    assert!(
        validate_draft(&draft).is_err(),
        "empty remote must not convert"
    );
}

#[test]
fn non_local_kinds_are_refused_with_the_v2_note() {
    for kind_index in [KIND_INDEX_REMOTE, KIND_INDEX_DYNAMIC] {
        let mut draft = sample_draft();
        draft.kind_index = kind_index;
        assert_eq!(validate_draft(&draft), Err(V2_NOTE.to_string()));
    }
}

#[test]
fn rules_from_drafts_reports_per_row_errors() {
    let mut bad = sample_draft();
    bad.bind_port = "nan".to_string();
    let Err(errors) = rules_from_drafts(&[sample_draft(), bad]) else {
        panic!("one bad row must fail the batch");
    };
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0], None);
    assert!(errors[1].is_some(), "the failing row carries the message");
}

#[test]
fn status_cells() {
    assert_eq!(
        status_text(Some(&ForwardState::Listening), true),
        "Listening"
    );
    assert_eq!(
        status_text(Some(&ForwardState::Active { conns: 1 }), true),
        "1 connection"
    );
    assert_eq!(
        status_text(Some(&ForwardState::Active { conns: 3 }), true),
        "3 connections"
    );
    assert_eq!(
        status_text(
            Some(&ForwardState::LastFailed {
                error: "refused".to_string()
            }),
            true
        ),
        "Error: refused"
    );
    assert_eq!(status_text(Some(&ForwardState::Stopped), true), "Stopped");
    assert_eq!(status_text(None, true), "Stopped");
    // Offline every cell is the fallback, whatever the engine last said.
    assert_eq!(
        status_text(Some(&ForwardState::Listening), false),
        OFFLINE_STATUS
    );
    assert_eq!(status_text(None, false), OFFLINE_STATUS);
}

#[test]
fn fake_source_snapshot_emit_and_toggles() {
    let source = FakeStatusSource::new(vec![ForwardState::Listening, ForwardState::Stopped]);
    let snapshot = source.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].state, ForwardState::Listening);

    source.emit(ForwardEvent {
        rule_id: 0,
        state: ForwardState::Active { conns: 4 },
    });
    assert_eq!(
        lookup_status(&source.snapshot(), 0),
        Some(&ForwardState::Active { conns: 4 })
    );

    source.set_rule_enabled(1, true);
    assert_eq!(source.toggles(), vec![(1, true)]);
    assert_eq!(
        lookup_status(&source.snapshot(), 1),
        Some(&ForwardState::Listening),
        "enabling re-listens"
    );
    source.set_rule_enabled(0, false);
    assert_eq!(source.toggles(), vec![(1, true), (0, false)]);
    assert_eq!(
        lookup_status(&source.snapshot(), 0),
        Some(&ForwardState::Stopped),
        "disabling stops"
    );
}

#[test]
fn fake_source_broadcasts_to_subscribers() {
    // The dialog's drain path, without a display: subscribe, emit, and the
    // update is already queued (unbounded `send` never blocks, `try_recv`
    // never waits).
    let source = FakeStatusSource::new(vec![ForwardState::Listening]);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    source.subscribe(tx);
    let update = ForwardEvent {
        rule_id: 0,
        state: ForwardState::Active { conns: 2 },
    };
    source.emit(update.clone());
    assert_eq!(rx.try_recv(), Ok(update));
}

// --- EngineStatusBridge (engine→dialog adapter) ---------------------------

/// An engine whose channel opens always fail: no sockets, no sshd, just
/// the event stream the bridge consumes.
fn closed_engine() -> (ForwardEngine, tokio::sync::mpsc::Receiver<ForwardEvent>) {
    let opener = std::sync::Arc::new(|_host: String, _port: u16| {
        Box::pin(async move {
            Err(TransportError::ChannelError(
                "no channels in test".to_string(),
            ))
        }) as OpenerFuture
    });
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    (ForwardEngine::with_opener(opener, tx), rx)
}

fn remote_rule() -> ForwardRule {
    ForwardRule {
        kind: ForwardKind::Remote,
        bind_addr: "127.0.0.1".to_string(),
        bind_port: 9999,
        remote_host: "db.lan".to_string(),
        remote_port: 22,
        enabled: true,
    }
}

#[test]
fn bridge_snapshots_engine_states_by_rule_id() {
    let (engine, _rx) = closed_engine();
    let bridge = EngineStatusBridge::new(engine);
    assert!(bridge.snapshot().is_empty(), "nothing reported yet");

    // Out of order on purpose: the snapshot sorts by rule index.
    bridge.handle_event(ForwardEvent {
        rule_id: 2,
        state: ForwardState::Active { conns: 3 },
    });
    bridge.handle_event(ForwardEvent {
        rule_id: 0,
        state: ForwardState::LastFailed {
            error: "refused".to_string(),
        },
    });
    bridge.handle_event(ForwardEvent {
        rule_id: 1,
        state: ForwardState::Listening,
    });
    bridge.handle_event(ForwardEvent {
        rule_id: 3,
        state: ForwardState::Stopped,
    });
    assert_eq!(
        bridge.snapshot(),
        vec![
            ForwardEvent {
                rule_id: 0,
                state: ForwardState::LastFailed {
                    error: "refused".to_string(),
                },
            },
            ForwardEvent {
                rule_id: 1,
                state: ForwardState::Listening,
            },
            ForwardEvent {
                rule_id: 2,
                state: ForwardState::Active { conns: 3 },
            },
            ForwardEvent {
                rule_id: 3,
                state: ForwardState::Stopped,
            },
        ]
    );

    // A repeated event moves the cached entry, it does not duplicate it.
    bridge.handle_event(ForwardEvent {
        rule_id: 2,
        state: ForwardState::Stopped,
    });
    let snapshot = bridge.snapshot();
    assert_eq!(snapshot.len(), 4);
    assert_eq!(lookup_status(&snapshot, 2), Some(&ForwardState::Stopped));
}

#[test]
fn bridge_snapshot_leaves_unreported_rules_to_the_stopped_fallback() {
    let (engine, _rx) = closed_engine();
    let bridge = EngineStatusBridge::new(engine);
    bridge.handle_event(ForwardEvent {
        rule_id: 0,
        state: ForwardState::Listening,
    });
    let snapshot = bridge.snapshot();
    assert_eq!(snapshot.len(), 1);
    // Per the trait docs the dialog renders a missing entry as Stopped.
    assert_eq!(lookup_status(&snapshot, 1), None);
    assert_eq!(status_text(lookup_status(&snapshot, 1), true), "Stopped");
}

#[test]
fn bridge_fans_out_and_prunes_dead_subscribers() {
    let (engine, _rx) = closed_engine();
    let bridge = EngineStatusBridge::new(engine);
    let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
    let (tx2, rx2) = tokio::sync::mpsc::unbounded_channel();
    bridge.subscribe(tx1);
    bridge.subscribe(tx2);
    drop(rx2);

    let first = ForwardEvent {
        rule_id: 0,
        state: ForwardState::Listening,
    };
    bridge.handle_event(ForwardEvent {
        rule_id: first.rule_id,
        state: ForwardState::Listening,
    });
    assert_eq!(rx1.try_recv(), Ok(first));

    // The dead sender was pruned on that send: later updates still reach
    // the live subscriber instead of accumulating dead entries.
    let second = ForwardEvent {
        rule_id: 0,
        state: ForwardState::Stopped,
    };
    bridge.handle_event(ForwardEvent {
        rule_id: second.rule_id,
        state: ForwardState::Stopped,
    });
    assert_eq!(rx1.try_recv(), Ok(second));
    assert!(rx1.try_recv().is_err(), "no duplicate deliveries");
}

#[tokio::test]
async fn bridge_set_rule_enabled_is_queued_and_reaches_the_engine() {
    let (engine, mut rx) = closed_engine();
    // A Remote rule never binds a socket (skipped with Stopped), so
    // this toggle exercises the full engine path with no network.
    engine.start(vec![remote_rule()]);
    assert!(matches!(
        timeout_recv(&mut rx).await.state,
        ForwardState::Stopped
    ));
    let bridge = EngineStatusBridge::new(engine);

    // Returns at once (queued, never blocking the GTK thread); the
    // resulting state arrives back through the event drain.
    bridge.set_rule_enabled(0, false);
    bridge.set_rule_enabled(0, true);
    assert_eq!(
        timeout_recv(&mut rx).await,
        ForwardEvent {
            rule_id: 0,
            state: ForwardState::Stopped,
        },
        "re-enabling a skipped Remote rule must report Stopped again"
    );

    // Unknown ids are ignored by the engine, never a panic or a hang.
    bridge.set_rule_enabled(99, true);
}

async fn timeout_recv(rx: &mut tokio::sync::mpsc::Receiver<ForwardEvent>) -> ForwardEvent {
    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for a forward event")
        .expect("event channel closed")
}
