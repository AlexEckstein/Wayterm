//! Display-free tests: the store roundtrip through drafts, the validation
//! the dialogs share, the status summaries the rows render, and the fake
//! source the integrator's bridge replaces.

use super::model::{
    apply_status, draft_from_rule, drafts_from_rules, kind_from_index, kind_index_for,
    lookup_status, rules_from_drafts, status_text, summarize_rules, validate_draft,
    ForwardRuleDraft, KIND_INDEX_DYNAMIC, KIND_INDEX_LOCAL, KIND_INDEX_REMOTE, OFFLINE_STATUS,
    V2_NOTE,
};
use super::status_source::{FakeStatusSource, ForwardState, ForwardStatus, ForwardStatusSource};
use crate::store::model::{ForwardKind, ForwardRule};

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
            Some(&ForwardState::Failed {
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
fn summaries_overlay_status_and_gate_engine_controls() {
    let rules = vec![
        sample_rule(),
        ForwardRule {
            kind: ForwardKind::Remote,
            enabled: false,
            ..sample_rule()
        },
    ];
    let statuses = vec![ForwardStatus {
        rule_index: 0,
        state: ForwardState::Active { conns: 2 },
    }];

    let rows = summarize_rules(&rules, &statuses, true);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].bind, "127.0.0.1:8080");
    assert_eq!(rows[0].remote, "web1.lan:80");
    assert_eq!(rows[0].status, "2 connections");
    assert!(rows[0].engine_control, "local rows drive the engine");
    assert_eq!(rows[1].kind_label, "Remote (v2)");
    assert_eq!(rows[1].status, "Stopped", "unreported rules read stopped");
    assert!(!rows[1].engine_control, "v2 rows never arm a control");

    let rows = summarize_rules(&rules, &statuses, false);
    assert!(rows.iter().all(|row| row.status == OFFLINE_STATUS));
    assert!(rows.iter().all(|row| !row.engine_control));
}

#[test]
fn apply_status_folds_updates_into_the_snapshot() {
    let mut statuses = vec![ForwardStatus {
        rule_index: 0,
        state: ForwardState::Listening,
    }];
    apply_status(
        &mut statuses,
        ForwardStatus {
            rule_index: 0,
            state: ForwardState::Active { conns: 1 },
        },
    );
    assert_eq!(statuses.len(), 1);
    assert_eq!(
        lookup_status(&statuses, 0),
        Some(&ForwardState::Active { conns: 1 })
    );
    // A rule the dialog did not know yet grows the snapshot.
    apply_status(
        &mut statuses,
        ForwardStatus {
            rule_index: 2,
            state: ForwardState::Failed {
                error: "down".to_string(),
            },
        },
    );
    assert_eq!(statuses.len(), 2);
    assert!(lookup_status(&statuses, 1).is_none());
}

#[test]
fn fake_source_snapshot_emit_and_toggles() {
    let source = FakeStatusSource::new(vec![ForwardState::Listening, ForwardState::Stopped]);
    let snapshot = source.snapshot();
    assert_eq!(snapshot.len(), 2);
    assert_eq!(snapshot[0].state, ForwardState::Listening);

    source.emit(ForwardStatus {
        rule_index: 0,
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
    let update = ForwardStatus {
        rule_index: 0,
        state: ForwardState::Active { conns: 2 },
    };
    source.emit(update.clone());
    assert_eq!(rx.try_recv(), Ok(update));
}
