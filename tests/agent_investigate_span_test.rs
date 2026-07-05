//! Fake-sink span assertions for the AG-23 investigate-run span
//! (`agent.investigate.run`, plan.md Task A8 / `agent.md` §6 :1335).
//!
//! Lives in its own integration-test binary for the same reason as
//! `tests/agent_review_span_test.rs`: the assertions install a
//! thread-local `tracing` subscriber, and tracing's per-callsite interest
//! cache can flap when sibling threads in the same process evaluate the
//! same callsites without a subscriber — a single-test binary removes
//! that concurrency by construction.
//!
//! Contract under test (§6 table): the span carries `run_id`, `turn`,
//! `next_agent_idx`, `terminal_state`; the untrusted seed topic and the
//! investigator's raw stdout are FORBIDDEN fields and must NEVER reach
//! the sink.

#![cfg(unix)]

use std::{path::PathBuf, sync::Mutex, time::Duration};

use libra::internal::ai::{
    investigate::{
        InvestigateCancelHandle, InvestigateRunRequest, InvestigateRunStore,
        InvestigateTerminalState, InvestigatorSource, run_investigate,
    },
    review::ReviewerCommand,
};

/// Shared in-memory sink handed to the fmt subscriber (identical to the
/// pattern in `tests/agent_review_span_test.rs`).
#[derive(Clone, Default)]
struct Sink(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn capture_spans<F: FnOnce()>(f: F) -> String {
    let sink = Sink::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(sink.clone())
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let captured = sink.0.lock().unwrap();
    String::from_utf8_lossy(&captured).to_string()
}

/// Single-threaded runtime so the run loop polls on this thread, where
/// the thread-local subscriber is installed. (`spawn_blocking` work —
/// workspace materialization — runs on blocking threads whose events
/// fall to the no-op global default; the `agent.investigate.run` span
/// itself opens, records, and closes on this thread.)
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread tokio runtime")
}

/// A completed investigate run emits one `agent.investigate.run` span
/// carrying the §6 required fields (`run_id`, `turn`, `next_agent_idx`,
/// `terminal_state`); neither the untrusted seed topic nor the
/// investigator's raw stdout — both of which demonstrably flowed through
/// the run into `findings.md` — ever reaches the sink.
#[test]
fn investigate_run_span_carries_required_fields_without_seed_or_reviewer_text() {
    let rt = runtime();
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo dir");
    std::fs::write(repo.join("README.md"), "span test repo\n").expect("seed file");
    let store = InvestigateRunStore::new(dir.path().join(".libra").join("sessions"));

    // Two unique markers: if either reaches the sink, the span (or an
    // event in the run loop) leaked forbidden content.
    let topic_marker = "TOPIC-SEED-MARKER-9x";
    let stdout_marker = "INVESTIGATOR-STDOUT-MARKER-9x";
    let investigator = InvestigatorSource::Custom(ReviewerCommand {
        slug: "inv-span".to_string(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".to_string(),
            format!("printf '%s\\n' '{stdout_marker} conclude: found it'"),
        ],
        env: Vec::new(),
        timeout: Duration::from_secs(30),
    });
    let request = InvestigateRunRequest::new(
        &repo,
        format!("investigate {topic_marker}"),
        "sha-span",
        vec![investigator],
        4,
        1,
    );

    let mut outcome_slot = None;
    let captured = capture_spans(|| {
        let outcome = rt
            .block_on(run_investigate(
                &store,
                request,
                InvestigateCancelHandle::new(),
            ))
            .expect("investigate run completes");
        outcome_slot = Some(outcome);
    });
    let outcome = outcome_slot.expect("run produced an outcome");
    assert_eq!(
        outcome.terminal_state,
        Some(InvestigateTerminalState::Quorum)
    );

    // Both markers really flowed through the run (so their absence from
    // the span sink below is meaningful, not vacuous).
    let findings = store
        .read_findings(&outcome.run_id)
        .expect("read findings")
        .expect("findings exist");
    assert!(
        findings.contains(stdout_marker),
        "investigator stdout must reach findings.md: {findings}"
    );
    assert!(
        findings.contains(topic_marker),
        "the seed topic must reach findings.md: {findings}"
    );

    assert!(
        captured.contains("agent.investigate.run"),
        "investigate run span missing: {captured}"
    );
    for field in [
        format!("run_id=\"{}\"", outcome.run_id),
        "turn=1".to_string(),
        "next_agent_idx=0".to_string(),
        "terminal_state=\"quorum\"".to_string(),
    ] {
        assert!(
            captured.contains(&field),
            "investigate run span missing `{field}`: {captured}"
        );
    }

    // Forbidden content: the untrusted seed topic and investigator stdout.
    assert!(
        !captured.contains(topic_marker),
        "the seed topic must never reach the span sink: {captured}"
    );
    assert!(
        !captured.contains(stdout_marker),
        "investigator stdout must never reach the span sink: {captured}"
    );
}
