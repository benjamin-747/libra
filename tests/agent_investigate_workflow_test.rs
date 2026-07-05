//! AG-23 read-only agent investigate workflow tests
//! (`docs/development/tracing/plan.md` Task A8; `agent.md` E8-libra +
//! 落地执行补充规格 §5 / test-matrix pins :1767).
//!
//! **Layer:** L1 — deterministic. Investigators are fake `/bin/sh`
//! scripts from `tests/fixtures/agent_workflows/` (see the provenance
//! README there), driven through the `InvestigatorSource::Custom` /
//! `ReviewerCommand` test seam — no network, no credentials, no real
//! agent CLIs. Investigate reuses A7's launcher/sink/redaction/isolation,
//! so the fake-investigator seam is the review `ReviewerCommand` verbatim.
//!
//! CLI-surface scenarios that cannot be driven with a `Custom` seam
//! (`investigate fix`, `investigate list` pagination) go through the real
//! binary (`CARGO_BIN_EXE_libra`) with an isolated `HOME`, matching the
//! `tests/command/mod.rs` / `agent_review_workflow_test` helper shape and
//! seeding runs directly through the store.
//!
//! Pinned scenario names (agent.md test matrix, AG-23 / E8 row :1767):
//! - `round_robin_reaches_quorum_and_max_turns`
//! - `stalled_cancelled_paused_and_continue_resume_are_pinned`
//! - `investigate_read_only_persists_state_and_findings_doc`
//! - `investigate_fix_returns_unsupported_until_bridge_ready`
//! - `concurrent_same_run_id_fails_closed`
//!
//! plus the 强制补强项 #5 keyset-pagination envelope through the real CLI
//! (`investigate_list_cli_paginates_with_keyset_cursor_envelope`).
//! (`investigate_fix_bridge_enters_agent_runtime_mutating_path` is the
//! matrix's fix-bridge alternative; it only lands once the internal fix
//! bridge has a source anchor — until then the unsupported pin below is
//! the mandatory one.)
//!
//! No test changes the process working directory or process environment,
//! so none of them need `#[serial]`.

#![cfg(unix)]

use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

use libra::internal::ai::{
    investigate::{
        DEFAULT_CLAUDE_REVIEW_MAX_BUDGET_USD, DEFAULT_INVESTIGATOR_TIMEOUT,
        InvestigateCancelHandle, InvestigateRunError, InvestigateRunOutcome, InvestigateRunRequest,
        InvestigateRunStore, InvestigateTerminalState, InvestigatorSource, PauseReason,
        StanceDisposition, continue_investigate_with_sources, run_investigate,
    },
    review::{ReviewerCommand, UNTRUSTED_FINDINGS_CLOSE, UNTRUSTED_FINDINGS_OPEN_PREFIX},
};

/// The fake `sk-` credential `investigator-secret.sh` assembles at run
/// time (never a literal in the fixture); it must never survive redaction.
const FAKE_CREDENTIAL: &str = "sk-abcdefghijklmnopqrstuvwx123456";

// ---------------------------------------------------------------------------
// Helpers: real-CLI invocation (isolated HOME), repo + fixture staging
// ---------------------------------------------------------------------------

/// Run the Libra binary with an isolated HOME so host config never leaks
/// into tests (`tests/command/mod.rs::base_libra_command` shape).
fn run_libra(args: &[&str], cwd: &Path, extra_env: &[(&str, &str)]) -> Output {
    let home = cwd.parent().unwrap_or(cwd).join(".libra-test-home");
    let config_home = home.join(".config");
    let global_db = home.join(".libra").join("config.db");
    std::fs::create_dir_all(&config_home).expect("create isolated config dir");

    let mut command = Command::new(env!("CARGO_BIN_EXE_libra"));
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("LIBRA_CONFIG_GLOBAL_DB", &global_db)
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env(libra::utils::pager::LIBRA_TEST_ENV, "1");
    if let Some(llvm_profile_file) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", llvm_profile_file);
    }
    for (key, value) in extra_env {
        command.env(key, value);
    }
    command.output().expect("failed to execute libra binary")
}

fn assert_cli_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `libra init` a fresh repository under `<root>/repo`.
fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("create repo dir");
    assert_cli_success(&run_libra(&["init"], &repo, &[]), "libra init");
    repo
}

/// A real Libra repository with one commit, so `repo_root` has a valid
/// HEAD and the mandatory isolated-workspace copy mirrors real content.
fn init_committed_repo(root: &Path) -> PathBuf {
    let repo = init_repo(root);
    assert_cli_success(
        &run_libra(
            &["config", "user.name", "Libra Investigate Test"],
            &repo,
            &[],
        ),
        "config user.name",
    );
    assert_cli_success(
        &run_libra(
            &["config", "user.email", "investigate-test@example.com"],
            &repo,
            &[],
        ),
        "config user.email",
    );
    std::fs::write(repo.join("tracked.txt"), "tracked content\n").expect("seed tracked file");
    assert_cli_success(&run_libra(&["add", "tracked.txt"], &repo, &[]), "libra add");
    assert_cli_success(
        &run_libra(&["commit", "-m", "base", "--no-verify"], &repo, &[]),
        "libra commit",
    );
    repo
}

/// The run store the engine and the CLI share for a repo
/// (`<repo>/.libra/sessions/agent-runs/<run_id>/`).
fn store_for(repo: &Path) -> InvestigateRunStore {
    InvestigateRunStore::new(repo.join(".libra").join("sessions"))
}

/// Copy a fixture investigator script into `dir` and (re-)apply `0o755`
/// so a checkout that dropped file modes cannot break the suite.
fn stage_fixture(dir: &Path, name: &str) -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("agent_workflows")
        .join(name);
    std::fs::create_dir_all(dir).expect("create fixture staging dir");
    let target = dir.join(name);
    std::fs::copy(&source, &target)
        .unwrap_or_else(|error| panic!("failed to stage fixture {name}: {error}"));
    let mut perms = std::fs::metadata(&target)
        .expect("fixture metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&target, perms).expect("fixture chmod");
    target
}

/// Directly constructed `InvestigatorSource::Custom` — the documented
/// test seam. The environment is deliberately EMPTY: fixtures must
/// survive the production `env_clear()` spawn contract. The engine
/// unconditionally runs every investigator inside the isolated workspace.
fn fake_investigator(
    slug: &str,
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> InvestigatorSource {
    InvestigatorSource::Custom(ReviewerCommand {
        slug: slug.to_string(),
        program: program.to_path_buf(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        env: Vec::new(),
        timeout,
    })
}

/// Drive one run to a terminal state / pause under a firm test deadline —
/// a hang is a test failure, never a stuck CI job.
async fn run_bounded(
    store: &InvestigateRunStore,
    request: InvestigateRunRequest,
    cancel: InvestigateCancelHandle,
    deadline_secs: u64,
) -> InvestigateRunOutcome {
    tokio::time::timeout(
        Duration::from_secs(deadline_secs),
        run_investigate(store, request, cancel),
    )
    .await
    .expect("investigate run must finish within the test deadline")
    .expect("investigate run reaches a terminal state or pause")
}

/// Assert no `libra-task-worktree-*` workspace is left under the repo's
/// task-worktree base after a run — the workspace lease was released.
fn assert_no_leaked_workspace(repo: &Path) {
    let tasks_dir = repo.join(".libra").join("worktrees").join("tasks");
    if !tasks_dir.exists() {
        return;
    }
    let leaked: Vec<String> = std::fs::read_dir(&tasks_dir)
        .expect("read task worktree dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("libra-task-worktree-"))
        .collect();
    assert!(
        leaked.is_empty(),
        "investigate run leaked isolated workspaces: {leaked:?}"
    );
}

// ---------------------------------------------------------------------------
// Pinned scenario 1: strict round-robin → quorum / max-turns
// ---------------------------------------------------------------------------

/// Two sub-scenarios pinned by one matrix name:
///
/// A. Investigators that emit concluding stances reach terminal `quorum`
///    with the agents advanced in STRICT round-robin order.
/// B. Investigators that never conclude exhaust `max_turns` and terminate
///    `max_turns` — the per-turn agent sequence proving round-robin
///    wraps across rounds (a, b, a).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn round_robin_reaches_quorum_and_max_turns() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");
    let cont = stage_fixture(&fixtures, "investigator-continue.sh");

    // ---- A: both investigators conclude; quorum 2 → terminal quorum. ----
    let request = InvestigateRunRequest::new(
        &repo,
        "why is startup slow",
        "sha-quorum",
        vec![
            fake_investigator("inv-a", &conclude, &[], Duration::from_secs(60)),
            fake_investigator("inv-b", &conclude, &[], Duration::from_secs(60)),
        ],
        6,
        2,
    );
    let outcome = run_bounded(&store, request, InvestigateCancelHandle::new(), 120).await;
    assert_eq!(
        outcome.terminal_state,
        Some(InvestigateTerminalState::Quorum)
    );
    assert_eq!(outcome.pause_reason, None);
    assert_eq!(outcome.concluding_count, 2);
    assert_eq!(outcome.turns_executed, 2);

    let state = store
        .load_state(&outcome.run_id)
        .expect("load state")
        .expect("state exists");
    // STRICT round-robin: exactly one turn per agent, agents in order.
    assert_eq!(state.stances.len(), 2);
    assert_eq!(state.stances[0].slug, "inv-a");
    assert_eq!(state.stances[0].agent_idx, 0);
    assert_eq!(state.stances[1].slug, "inv-b");
    assert_eq!(state.stances[1].agent_idx, 1);
    assert!(
        state
            .stances
            .iter()
            .all(|s| s.disposition == StanceDisposition::Concluding)
    );
    // state.json round-robin bookkeeping.
    assert_eq!(state.turn, 2);
    assert_eq!(state.completed_rounds, 1);
    assert_eq!(state.next_agent_idx, 0, "advanced past b, wrapped to 0");
    assert_eq!(state.max_turns, 6);
    assert_eq!(state.quorum, 2);
    assert_eq!(state.starting_sha, "sha-quorum");
    assert_eq!(state.terminal_state, Some(InvestigateTerminalState::Quorum));

    // findings.md reflects the converged run and both concluding stances.
    let findings = store
        .read_findings(&outcome.run_id)
        .expect("read findings")
        .expect("findings exist");
    assert!(findings.contains("# Investigation findings"), "{findings}");
    assert!(findings.contains("status: quorum"), "{findings}");
    assert!(findings.contains("cache.rs:42"), "{findings}");
    assert_no_leaked_workspace(&repo);

    // ---- B: neither investigator concludes; max_turns 3 → terminal max_turns. ----
    let request = InvestigateRunRequest::new(
        &repo,
        "why is startup slow",
        "sha-maxturns",
        vec![
            fake_investigator("inv-a", &cont, &[], Duration::from_secs(60)),
            fake_investigator("inv-b", &cont, &[], Duration::from_secs(60)),
        ],
        3,
        2,
    );
    let outcome = run_bounded(&store, request, InvestigateCancelHandle::new(), 120).await;
    assert_eq!(
        outcome.terminal_state,
        Some(InvestigateTerminalState::MaxTurns)
    );
    assert_eq!(outcome.turns_executed, 3);
    assert_eq!(outcome.concluding_count, 0);

    let state = store
        .load_state(&outcome.run_id)
        .expect("load state")
        .expect("state exists");
    // Round-robin wraps across rounds: a, b, a (agent indices 0, 1, 0).
    let sequence: Vec<(&str, usize)> = state
        .stances
        .iter()
        .map(|s| (s.slug.as_str(), s.agent_idx))
        .collect();
    assert_eq!(
        sequence,
        vec![("inv-a", 0), ("inv-b", 1), ("inv-a", 0)],
        "strict round-robin must preserve agent order across rounds"
    );
    assert!(
        state
            .stances
            .iter()
            .all(|s| s.disposition == StanceDisposition::Continuing)
    );
    assert_eq!(state.completed_rounds, 1);
    assert_no_leaked_workspace(&repo);
}

// ---------------------------------------------------------------------------
// Pinned scenario 2: stall / agent-failure pause, cancel, continue-resume
// ---------------------------------------------------------------------------

/// The pause/cancel/resume half of the matrix in one pinned name:
///
/// - a silent-but-successful turn PAUSES the run as `stalled` with a
///   `pending_turn` (non-terminal), and `continue` resumes it to a
///   terminal `quorum`;
/// - a non-zero investigator PAUSES the run as `agent_failure` with the
///   failed turn recorded for retry;
/// - a cancel drives the run to a terminal `cancelled` promptly, with the
///   isolated workspace released and the terminal state persisted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_cancelled_paused_and_continue_resume_are_pinned() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let silent = stage_fixture(&fixtures, "investigator-silent.sh");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");
    let error = stage_fixture(&fixtures, "reviewer-error.sh");
    let slow = stage_fixture(&fixtures, "reviewer-slow.sh");

    // ---- Stall → paused (stalled), pending_turn set, non-terminal. ----
    let request = InvestigateRunRequest::new(
        &repo,
        "why is startup slow",
        "sha-stall",
        vec![fake_investigator(
            "inv-a",
            &silent,
            &[],
            Duration::from_secs(60),
        )],
        4,
        1,
    );
    let outcome = run_bounded(&store, request, InvestigateCancelHandle::new(), 120).await;
    assert_eq!(outcome.terminal_state, None, "a stall pauses, not terminal");
    assert_eq!(outcome.pause_reason, Some(PauseReason::Stalled));
    let stall_run_id = outcome.run_id.clone();
    let state = store
        .load_state(&stall_run_id)
        .expect("load state")
        .expect("state exists");
    assert!(state.is_paused());
    let pending = state.pending_turn.as_ref().expect("pending turn recorded");
    assert_eq!(pending.reason, PauseReason::Stalled);
    assert_eq!(pending.turn, 1);
    assert_eq!(pending.agent_idx, 0);
    assert_eq!(state.turn, 0, "the stalled turn produced no stance");
    assert_no_leaked_workspace(&repo);

    // ---- Continue resumes from the pending turn → terminal quorum. ----
    let resumed = continue_investigate_with_sources(
        &store,
        &stall_run_id,
        vec![fake_investigator(
            "inv-a",
            &conclude,
            &[],
            Duration::from_secs(60),
        )],
        &repo,
        DEFAULT_INVESTIGATOR_TIMEOUT,
        true,
        DEFAULT_CLAUDE_REVIEW_MAX_BUDGET_USD,
        InvestigateCancelHandle::new(),
    )
    .await
    .expect("continue resumes a paused run");
    assert_eq!(
        resumed.terminal_state,
        Some(InvestigateTerminalState::Quorum)
    );
    assert_eq!(resumed.turns_executed, 1);
    let state = store
        .load_state(&stall_run_id)
        .expect("load state")
        .expect("state exists");
    assert!(state.pending_turn.is_none(), "resume clears pending_turn");
    assert_eq!(state.terminal_state, Some(InvestigateTerminalState::Quorum));
    assert_no_leaked_workspace(&repo);

    // ---- Agent failure (non-zero exit) → paused (agent_failure). ----
    let request = InvestigateRunRequest::new(
        &repo,
        "why is startup slow",
        "sha-fail",
        vec![fake_investigator(
            "inv-fail",
            &error,
            &[],
            Duration::from_secs(60),
        )],
        4,
        1,
    );
    let outcome = run_bounded(&store, request, InvestigateCancelHandle::new(), 120).await;
    assert_eq!(outcome.terminal_state, None);
    assert_eq!(outcome.pause_reason, Some(PauseReason::AgentFailure));
    let state = store
        .load_state(&outcome.run_id)
        .expect("load state")
        .expect("state exists");
    let pending = state.pending_turn.as_ref().expect("pending turn recorded");
    assert_eq!(pending.reason, PauseReason::AgentFailure);
    assert_eq!(pending.turn, 1);
    assert_eq!(pending.agent_idx, 0);
    assert!(
        pending.detail.is_some(),
        "the agent-failure pause records a retry detail"
    );
    assert_no_leaked_workspace(&repo);

    // ---- Cancel → terminal cancelled promptly, workspace released. ----
    let request = InvestigateRunRequest::new(
        &repo,
        "why is startup slow",
        "sha-cancel",
        vec![fake_investigator(
            "inv-sleeper",
            &slow,
            &["30"],
            Duration::from_secs(120),
        )],
        4,
        1,
    );
    let cancel = InvestigateCancelHandle::new();
    let started = Instant::now();
    let run = tokio::spawn({
        let store = store.clone();
        let cancel = cancel.clone();
        async move { run_investigate(&store, request, cancel).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("cancelled run must finish promptly")
        .expect("join run task")
        .expect("run reaches a terminal state");
    assert_eq!(
        outcome.terminal_state,
        Some(InvestigateTerminalState::Cancelled)
    );
    assert!(
        started.elapsed() < Duration::from_secs(25),
        "cancel must not wait out the 30s sleeper (took {:?})",
        started.elapsed()
    );
    let state = store
        .load_state(&outcome.run_id)
        .expect("load state")
        .expect("state exists");
    assert_eq!(
        state.terminal_state,
        Some(InvestigateTerminalState::Cancelled)
    );
    assert!(
        state.pending_turn.is_none(),
        "a cancel discards any pending resume point"
    );
    assert_no_leaked_workspace(&repo);
}

// ---------------------------------------------------------------------------
// Pinned scenario 3: read-only state + findings doc (E8 wire + redaction)
// ---------------------------------------------------------------------------

/// A read-only investigate run persists the E8-libra run wire exactly:
/// `manifest.json` with EXACTLY the 12 E8 keys and `kind = "investigate"`,
/// `state.json` carrying every round-robin field, and a `findings.md`
/// whose per-stance excerpts are spotlighting-delimited (provenance=
/// untrusted) and redacted — a fake `sk-` credential seeded into a stance
/// never survives into `findings.md` or the `*.redacted.log` files.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn investigate_read_only_persists_state_and_findings_doc() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let cont = stage_fixture(&fixtures, "investigator-continue.sh");
    let secret = stage_fixture(&fixtures, "investigator-secret.sh");

    // A unique marker in the topic (untrusted seed) so we can prove it is
    // persisted (control-scrubbed) into the findings document.
    let topic = "why is startup slow TOPIC-SEED-MARKER-7z";
    let request = InvestigateRunRequest::new(
        &repo,
        topic,
        "sha-e8-wire",
        vec![
            // Turn 1: a continuing stance (recorded, injected as prior
            // context into turn 2's spotlit prompt).
            fake_investigator("inv-a", &cont, &[], Duration::from_secs(60)),
            // Turn 2: a concluding stance that also emits the fake secret.
            fake_investigator("inv-secret", &secret, &[], Duration::from_secs(60)),
        ],
        4,
        1,
    );
    let outcome = run_bounded(&store, request, InvestigateCancelHandle::new(), 120).await;
    assert_eq!(
        outcome.terminal_state,
        Some(InvestigateTerminalState::Quorum)
    );
    let run_id = outcome.run_id.clone();

    // ---- manifest.json: exactly the 12 E8 keys, kind = investigate. ----
    let manifest_path = store.manifest_path(&run_id).expect("manifest path");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest_path).expect("read manifest"))
            .expect("manifest parses");
    let mut keys: Vec<&str> = manifest
        .as_object()
        .expect("manifest is a JSON object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    let mut expected = vec![
        "schema_version",
        "run_id",
        "kind",
        "agents",
        "starting_sha",
        "target_scope",
        "terminal_state",
        "created_at",
        "updated_at",
        "findings_oid",
        "redaction_report",
        "manual_attach",
    ];
    expected.sort_unstable();
    assert_eq!(
        keys, expected,
        "manifest.json must carry exactly the E8 keys"
    );
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["run_id"], run_id.as_str());
    assert_eq!(manifest["kind"], "investigate");
    assert_eq!(
        manifest["agents"],
        serde_json::json!(["inv-a", "inv-secret"])
    );
    assert_eq!(manifest["starting_sha"], "sha-e8-wire");
    assert_eq!(manifest["target_scope"], topic);
    assert_eq!(manifest["terminal_state"], "quorum");
    assert!(
        manifest["findings_oid"].is_null(),
        "AG-23 writes no findings object yet"
    );
    assert_eq!(
        manifest["manual_attach"],
        serde_json::json!([]),
        "manual_attach is an EMPTY placeholder in AG-23 (no command surface)"
    );
    assert!(
        manifest["redaction_report"]["matches"]
            .as_u64()
            .expect("matches is a number")
            >= 1,
        "the fake credential must have hit at least one redaction rule: {manifest}"
    );

    // ---- state.json: stances persisted with round-robin dispositions. ----
    let state = store
        .load_state(&run_id)
        .expect("load state")
        .expect("state exists");
    assert_eq!(state.kind, "investigate");
    assert_eq!(state.topic, topic, "the seed topic is stored verbatim");
    assert_eq!(state.stances.len(), 2);
    assert_eq!(state.stances[0].slug, "inv-a");
    assert_eq!(state.stances[0].disposition, StanceDisposition::Continuing);
    assert_eq!(state.stances[1].slug, "inv-secret");
    assert_eq!(state.stances[1].disposition, StanceDisposition::Concluding);

    // ---- findings.md: spotlit untrusted stances + redacted secret. ----
    let findings = store
        .read_findings(&run_id)
        .expect("read findings")
        .expect("findings exist");
    // The untrusted seed topic is persisted (control-scrubbed) in the header.
    assert!(
        findings.contains("TOPIC-SEED-MARKER-7z"),
        "the untrusted seed topic must appear in findings: {findings}"
    );
    // Every stance is fenced in spotlighting delimiters (provenance=untrusted).
    assert!(
        findings.contains(&format!(
            "{UNTRUSTED_FINDINGS_OPEN_PREFIX} slug=\"inv-a\">>>"
        )),
        "findings must open a spotlighting block per investigator: {findings}"
    );
    assert!(
        findings.contains(&format!(
            "{UNTRUSTED_FINDINGS_OPEN_PREFIX} slug=\"inv-secret\">>>"
        )),
        "findings must open a spotlighting block for the concluding stance: {findings}"
    );
    assert!(
        findings.contains(UNTRUSTED_FINDINGS_CLOSE),
        "findings must close the spotlighting block: {findings}"
    );
    assert!(
        findings.contains("leak confirmed in cache.rs"),
        "the concluding stance text must be captured: {findings}"
    );
    assert!(
        !findings.contains(FAKE_CREDENTIAL),
        "the fake credential must never survive redaction into findings.md: {findings}"
    );

    // ---- reviewers/<slug>.{stdout,stderr}.redacted.log: redacted, scrubbed. ----
    let stdout_log = store
        .investigator_stdout_log_path(&run_id, "inv-secret")
        .expect("stdout log path");
    assert!(stdout_log.is_file(), "stdout redacted log must exist");
    let stdout_text = std::fs::read_to_string(&stdout_log).expect("stdout log");
    assert!(
        stdout_text.contains("leak confirmed in cache.rs"),
        "{stdout_text}"
    );
    assert!(
        !stdout_text.contains(FAKE_CREDENTIAL),
        "the fake credential must never survive redaction into the log"
    );
    assert!(
        !stdout_text.contains('\u{1b}'),
        "persisted logs must be control-scrubbed (no raw ESC): {stdout_text:?}"
    );
    assert!(
        store
            .investigator_stderr_log_path(&run_id, "inv-secret")
            .expect("stderr log path")
            .is_file(),
        "stderr redacted log must exist"
    );

    // Read-only posture: the repo worktree is untouched, no workspace leaked.
    assert_eq!(
        std::fs::read_to_string(repo.join("tracked.txt")).expect("tracked file"),
        "tracked content\n",
        "an investigation must never mutate the repository worktree"
    );
    assert_no_leaked_workspace(&repo);
}

// ---------------------------------------------------------------------------
// Pinned scenario 4: `investigate fix` fails closed with LBR-AGENT-010
// ---------------------------------------------------------------------------

/// `libra investigate fix <run_id>` fails closed with the stable
/// `LBR-AGENT-010` code (exit 128) until the internal AgentRuntime fix
/// bridge lands — never fake success (plan.md:1002). The refusal names
/// the read-only alternative (`investigate show`).
#[test]
fn investigate_fix_returns_unsupported_until_bridge_ready() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(temp.path());

    // Human/default surface.
    let output = run_libra(&["investigate", "fix", "some-run-id"], &repo, &[]);
    assert_eq!(
        output.status.code(),
        Some(128),
        "fix must be a fatal (128) refusal: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("LBR-AGENT-010"),
        "stderr must carry the stable code: {stderr}"
    );
    assert!(
        stderr.contains("fix bridge"),
        "the refusal must name the missing fix bridge: {stderr}"
    );
    assert!(
        stderr.contains("investigate show"),
        "the refusal must point at the read-only alternative: {stderr}"
    );

    // Structured JSON error surface.
    let output = run_libra(
        &["investigate", "fix", "some-run-id"],
        &repo,
        &[("LIBRA_ERROR_JSON", "1")],
    );
    assert_eq!(output.status.code(), Some(128));
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let json_start = stderr
        .rfind("\n{")
        .map(|index| index + 1)
        .or_else(|| stderr.find('{'))
        .expect("structured stderr must carry a JSON error report");
    let report: serde_json::Value =
        serde_json::from_str(stderr[json_start..].trim()).expect("JSON error report parses");
    assert_eq!(report["error_code"], "LBR-AGENT-010");
    assert_eq!(report["exit_code"], 128);
}

// ---------------------------------------------------------------------------
// Pinned scenario 5: concurrent continue on the same run fails closed
// ---------------------------------------------------------------------------

/// The run-id OS lock makes a concurrent `continue` on the same run fail
/// closed (plan.md:997). Holding the run lock in-test (the engine API)
/// stands in for a second driver process: the continue must error
/// `RunLocked`; once the lock is released, the continue succeeds and
/// drives the run to a terminal state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_same_run_id_fails_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let silent = stage_fixture(&fixtures, "investigator-silent.sh");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");

    // Produce a paused (stalled) run so `continue` is a valid next step.
    let request = InvestigateRunRequest::new(
        &repo,
        "why is startup slow",
        "sha-lock",
        vec![fake_investigator(
            "inv-a",
            &silent,
            &[],
            Duration::from_secs(60),
        )],
        4,
        1,
    );
    let outcome = run_bounded(&store, request, InvestigateCancelHandle::new(), 120).await;
    let run_id = outcome.run_id.clone();
    assert!(
        store
            .load_state(&run_id)
            .expect("load")
            .expect("state")
            .is_paused()
    );

    // Snapshot the paused state.json BEFORE the losing continue (P1
    // regression: a continue that loses the flock must not have mutated
    // any state — the resume point must survive so the run stays
    // resumable if the active driver crashes).
    let state_path = store.state_path(&run_id).expect("state path");
    let before = std::fs::read(&state_path).expect("read state before");

    // Hold the run lock (simulating a concurrent driver); the continue
    // must fail closed with RunLocked.
    let lock = store.try_lock_run(&run_id).expect("hold run lock");
    let err = continue_investigate_with_sources(
        &store,
        &run_id,
        vec![fake_investigator(
            "inv-a",
            &conclude,
            &[],
            Duration::from_secs(60),
        )],
        &repo,
        DEFAULT_INVESTIGATOR_TIMEOUT,
        true,
        DEFAULT_CLAUDE_REVIEW_MAX_BUDGET_USD,
        InvestigateCancelHandle::new(),
    )
    .await
    .expect_err("a concurrent continue on the same run must fail closed");
    match err {
        InvestigateRunError::RunLocked { run_id: locked } => assert_eq!(locked, run_id),
        other => panic!("expected RunLocked, got {other:?}"),
    }

    // P1 regression: the losing continue left state.json byte-identical —
    // pending_turn is intact and the run is still resumable.
    let after = std::fs::read(&state_path).expect("read state after");
    assert_eq!(
        before, after,
        "a continue that lost the flock must leave state.json byte-identical"
    );
    let paused = store.load_state(&run_id).expect("load").expect("state");
    assert!(paused.is_paused(), "the run stays paused/resumable");
    assert_eq!(
        paused.pending_turn.expect("pending_turn intact").reason,
        PauseReason::Stalled
    );

    // Release the lock — the continue now succeeds and reaches quorum.
    drop(lock);
    let resumed = continue_investigate_with_sources(
        &store,
        &run_id,
        vec![fake_investigator(
            "inv-a",
            &conclude,
            &[],
            Duration::from_secs(60),
        )],
        &repo,
        DEFAULT_INVESTIGATOR_TIMEOUT,
        true,
        DEFAULT_CLAUDE_REVIEW_MAX_BUDGET_USD,
        InvestigateCancelHandle::new(),
    )
    .await
    .expect("continue succeeds once the lock is released");
    assert_eq!(
        resumed.terminal_state,
        Some(InvestigateTerminalState::Quorum)
    );
    assert_no_leaked_workspace(&repo);
}

// ---------------------------------------------------------------------------
// CLI pagination: unified keyset page envelope (agent.md 强制补强项 #5)
// ---------------------------------------------------------------------------

fn list_page(repo: &Path, extra: &[&str]) -> serde_json::Value {
    let mut args = vec!["investigate", "list", "--json"];
    args.extend_from_slice(extra);
    let output = run_libra(&args, repo, &[]);
    assert_cli_success(&output, "investigate list --json");
    serde_json::from_slice(&output.stdout).expect("investigate list stdout is JSON")
}

fn page_ids(page: &serde_json::Value) -> Vec<String> {
    page["data"]["items"]
        .as_array()
        .expect("items is an array")
        .iter()
        .map(|item| {
            item["run_id"]
                .as_str()
                .expect("run_id is a string")
                .to_string()
        })
        .collect()
}

/// `investigate list --json --limit --cursor` through the real CLI: the
/// unified `{schema_version, items, next_cursor, has_more}` envelope, a
/// full no-duplicate/no-loss cursor walk over more runs than one page
/// (including a same-timestamp `run_id DESC` tiebreak), and a malformed
/// cursor failing closed as one usage error.
#[test]
fn investigate_list_cli_paginates_with_keyset_cursor_envelope() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_repo(temp.path());
    let store = store_for(&repo);

    // Seed N=7 > limit=3 runs with controlled started_at values
    // (fixed-width RFC 3339 micros — the keyset contract). run-5/run-6
    // share a timestamp to exercise the run_id DESC tiebreak.
    for (run_id, started_at) in [
        ("run-1", "2026-07-01T00:00:00.000000Z"),
        ("run-2", "2026-07-02T00:00:00.000000Z"),
        ("run-3", "2026-07-03T00:00:00.000000Z"),
        ("run-4", "2026-07-04T00:00:00.000000Z"),
        ("run-5", "2026-07-05T00:00:00.000000Z"),
        ("run-6", "2026-07-05T00:00:00.000000Z"),
        ("run-7", "2026-07-06T00:00:00.000000Z"),
    ] {
        let mut state = store
            .create_run(
                run_id,
                "seed topic",
                &["codex".to_string()],
                4,
                1,
                "sha-seed",
            )
            .expect("seed run");
        state.started_at = started_at.to_string();
        state.updated_at = started_at.to_string();
        state.terminal_state = Some(InvestigateTerminalState::Quorum);
        store.write_state(&state).expect("write seeded state");
    }
    let expected_order = [
        "run-7", "run-6", "run-5", "run-4", "run-3", "run-2", "run-1",
    ];

    // ---- Page 1: envelope shape + newest-first order. ----
    let page1 = list_page(&repo, &["--limit", "3"]);
    assert_eq!(page1["ok"], true);
    assert_eq!(page1["command"], "investigate_list");
    let data = page1["data"].as_object().expect("data is an object");
    let mut envelope_keys: Vec<&str> = data.keys().map(String::as_str).collect();
    envelope_keys.sort_unstable();
    assert_eq!(
        envelope_keys,
        ["has_more", "items", "next_cursor", "schema_version"],
        "the page envelope must carry exactly the unified keys"
    );
    assert_eq!(page1["data"]["schema_version"], 1);
    assert_eq!(page1["data"]["has_more"], true);
    assert_eq!(page_ids(&page1), &expected_order[0..3]);
    let cursor1 = page1["data"]["next_cursor"]
        .as_str()
        .expect("next_cursor is an opaque string")
        .to_string();

    // ---- Page 2 via the opaque cursor (round-trip). ----
    let page2 = list_page(&repo, &["--limit", "3", "--cursor", &cursor1]);
    assert_eq!(page2["data"]["has_more"], true);
    assert_eq!(page_ids(&page2), &expected_order[3..6]);
    let cursor2 = page2["data"]["next_cursor"]
        .as_str()
        .expect("second next_cursor")
        .to_string();

    // ---- Final page: remainder, no further cursor. ----
    let page3 = list_page(&repo, &["--limit", "3", "--cursor", &cursor2]);
    assert_eq!(page3["data"]["has_more"], false);
    assert!(page3["data"]["next_cursor"].is_null());
    assert_eq!(page_ids(&page3), &expected_order[6..]);

    // ---- Whole walk: no duplicates, no loss. ----
    let mut walked: Vec<String> = Vec::new();
    walked.extend(page_ids(&page1));
    walked.extend(page_ids(&page2));
    walked.extend(page_ids(&page3));
    assert_eq!(walked, expected_order, "cursor walk must be exact");

    // ---- Default limit (50) returns everything in one page. ----
    let all = list_page(&repo, &[]);
    assert_eq!(all["data"]["has_more"], false);
    assert_eq!(page_ids(&all).len(), 7);

    // ---- Malformed cursor fails closed as one usage error. ----
    let output = run_libra(
        &["investigate", "list", "--cursor", "not-a-cursor!!"],
        &repo,
        &[],
    );
    assert_eq!(
        output.status.code(),
        Some(129),
        "a malformed cursor is a usage error: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid --cursor"),
        "the refusal must be actionable: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// P1 regression: `clean` refuses unfinished (running/paused) runs
// ---------------------------------------------------------------------------

/// Codex P1: `clean --run`/`--all` must NOT drop a PAUSED (resumable) run
/// and its findings. `--run` refuses a non-terminal run with an actionable
/// hint; `--all` skips non-terminal runs (reporting the count) and removes
/// only terminal ones; once cancelled, the run becomes cleanable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn investigate_clean_refuses_unfinished_runs_and_all_skips_them() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let silent = stage_fixture(&fixtures, "investigator-silent.sh");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");

    // A paused (resumable) run — its findings must survive `clean`.
    let paused = run_bounded(
        &store,
        InvestigateRunRequest::new(
            &repo,
            "keep me",
            "sha-keep",
            vec![fake_investigator(
                "inv-a",
                &silent,
                &[],
                Duration::from_secs(60),
            )],
            4,
            1,
        ),
        InvestigateCancelHandle::new(),
        120,
    )
    .await;
    assert!(
        store
            .load_state(&paused.run_id)
            .unwrap()
            .unwrap()
            .is_paused()
    );

    // A terminal (quorum) run — this one IS removable.
    let terminal = run_bounded(
        &store,
        InvestigateRunRequest::new(
            &repo,
            "done",
            "sha-done",
            vec![fake_investigator(
                "inv-a",
                &conclude,
                &[],
                Duration::from_secs(60),
            )],
            4,
            1,
        ),
        InvestigateCancelHandle::new(),
        120,
    )
    .await;
    assert_eq!(
        terminal.terminal_state,
        Some(InvestigateTerminalState::Quorum)
    );

    // `clean --run <paused>` refuses (exit 128, actionable).
    let output = run_libra(
        &["investigate", "clean", "--run", &paused.run_id],
        &repo,
        &[],
    );
    assert_eq!(
        output.status.code(),
        Some(128),
        "clean must refuse a paused run: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("has not finished") && stderr.contains("cancel"),
        "refusal must be actionable: {stderr}"
    );
    assert!(
        store.load_state(&paused.run_id).unwrap().is_some(),
        "the paused run must survive a refused clean --run"
    );

    // `clean --all` removes only the terminal run, skips the paused one.
    let output = run_libra(&["investigate", "clean", "--all", "--json"], &repo, &[]);
    assert_cli_success(&output, "investigate clean --all --json");
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("clean --all stdout is JSON");
    assert_eq!(
        report["data"]["removed"], 1,
        "only the terminal run removed"
    );
    assert_eq!(
        report["data"]["skipped_running"], 1,
        "the paused run must be skipped, not removed"
    );
    assert!(
        store.load_state(&paused.run_id).unwrap().is_some(),
        "the paused run must survive clean --all"
    );
    assert!(
        store.load_state(&terminal.run_id).unwrap().is_none(),
        "the terminal run was removed"
    );

    // After cancelling, `clean --run` removes it.
    assert_cli_success(
        &run_libra(&["investigate", "cancel", &paused.run_id], &repo, &[]),
        "investigate cancel",
    );
    assert_cli_success(
        &run_libra(
            &["investigate", "clean", "--run", &paused.run_id],
            &repo,
            &[],
        ),
        "investigate clean --run after cancel",
    );
    assert!(store.load_state(&paused.run_id).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// P1 regression: the run budget accumulates from the persisted started_at
// ---------------------------------------------------------------------------

/// Codex P1: the run-level timeout budget (`max_turns * 120s`, cap 3600s)
/// is measured from the PERSISTED `started_at`, so repeated `continue`
/// resumes cannot dodge the cap. A run whose start is backdated past the
/// cap terminates as `timeout` on the next continue, without running
/// another turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn investigate_run_budget_accumulates_across_continue() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let silent = stage_fixture(&fixtures, "investigator-silent.sh");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");

    // Pause the run (stall). max_turns 2 → run budget cap = 240s.
    let paused = run_bounded(
        &store,
        InvestigateRunRequest::new(
            &repo,
            "why is startup slow",
            "sha-budget",
            vec![fake_investigator(
                "inv-a",
                &silent,
                &[],
                Duration::from_secs(60),
            )],
            2,
            1,
        ),
        InvestigateCancelHandle::new(),
        120,
    )
    .await;
    assert_eq!(paused.pause_reason, Some(PauseReason::Stalled));

    // Backdate started_at well past the cap (simulating budget accumulated
    // across many resumes).
    let long_ago = chrono::Utc::now() - chrono::Duration::seconds(10_000);
    store
        .update_state(&paused.run_id, |state| {
            state.started_at = long_ago.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
        })
        .expect("backdate started_at");

    // A would-be-concluding resume must instead terminate as timeout,
    // never running another turn.
    let resumed = continue_investigate_with_sources(
        &store,
        &paused.run_id,
        vec![fake_investigator(
            "inv-a",
            &conclude,
            &[],
            Duration::from_secs(60),
        )],
        &repo,
        DEFAULT_INVESTIGATOR_TIMEOUT,
        true,
        DEFAULT_CLAUDE_REVIEW_MAX_BUDGET_USD,
        InvestigateCancelHandle::new(),
    )
    .await
    .expect("continue completes");
    assert_eq!(
        resumed.terminal_state,
        Some(InvestigateTerminalState::Timeout),
        "an over-budget resume must terminate as timeout"
    );
    assert_eq!(resumed.turns_executed, 0, "no turn ran once over budget");
    assert_no_leaked_workspace(&repo);
}

// ---------------------------------------------------------------------------
// R2 P1 regression: `clean --run` takes the lock even for corrupt state
// ---------------------------------------------------------------------------

/// Codex R2 P1: `clean --run` must acquire the run lock BEFORE deleting,
/// even when state.json is unreadable — a live run (holding `.lock`) whose
/// state is momentarily corrupt must not be deleted out from under its
/// driver. Holding the lock in-test stands in for a live driver process
/// (the CLI runs in a subprocess, so the flock genuinely contends).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_run_takes_the_lock_even_for_corrupt_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");

    // A terminal run, then corrupt its state.json (unreadable).
    let outcome = run_bounded(
        &store,
        InvestigateRunRequest::new(
            &repo,
            "corrupt me",
            "sha-corrupt",
            vec![fake_investigator(
                "inv-a",
                &conclude,
                &[],
                Duration::from_secs(60),
            )],
            4,
            1,
        ),
        InvestigateCancelHandle::new(),
        120,
    )
    .await;
    let run_id = outcome.run_id.clone();
    std::fs::write(store.state_path(&run_id).unwrap(), b"{ this is not json").unwrap();

    // Hold the run lock (simulating a live driver); `clean --run` must
    // refuse even though the state is corrupt (the corrupt-state allowance
    // must NOT bypass the lock).
    let lock = store.try_lock_run(&run_id).expect("hold run lock");
    let output = run_libra(&["investigate", "clean", "--run", &run_id], &repo, &[]);
    assert_eq!(
        output.status.code(),
        Some(128),
        "clean must refuse a locked (live) run even with corrupt state: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("being driven by another process"),
        "the refusal must say the run is live: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        store.run_dir(&run_id).unwrap().is_dir(),
        "the locked run must survive the refused clean"
    );

    // Release the lock: the corrupt (but lock-free) run is now cleanable.
    drop(lock);
    assert_cli_success(
        &run_libra(&["investigate", "clean", "--run", &run_id], &repo, &[]),
        "clean --run of a lock-free corrupt run",
    );
    assert!(!store.run_dir(&run_id).unwrap().exists());
}

// ---------------------------------------------------------------------------
// R2 P1 regression: continue on a terminal run is refused, state unchanged
// ---------------------------------------------------------------------------

/// Codex R2 P1: a `continue` on a terminal run is refused and MUST NOT
/// re-finalize it or run another turn — the persisted state is byte-for-
/// byte unchanged. (The locked TOCTOU re-check in `drive` is the last line
/// of defense; the pre-lock precheck returns the same `AlreadyTerminal`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continue_on_terminal_run_refused_leaves_state_unchanged() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");

    let outcome = run_bounded(
        &store,
        InvestigateRunRequest::new(
            &repo,
            "already done",
            "sha-terminal",
            vec![fake_investigator(
                "inv-a",
                &conclude,
                &[],
                Duration::from_secs(60),
            )],
            4,
            1,
        ),
        InvestigateCancelHandle::new(),
        120,
    )
    .await;
    assert_eq!(
        outcome.terminal_state,
        Some(InvestigateTerminalState::Quorum)
    );
    let before = std::fs::read(store.state_path(&outcome.run_id).unwrap()).unwrap();

    let err = continue_investigate_with_sources(
        &store,
        &outcome.run_id,
        vec![fake_investigator(
            "inv-a",
            &conclude,
            &[],
            Duration::from_secs(60),
        )],
        &repo,
        DEFAULT_INVESTIGATOR_TIMEOUT,
        true,
        DEFAULT_CLAUDE_REVIEW_MAX_BUDGET_USD,
        InvestigateCancelHandle::new(),
    )
    .await
    .expect_err("a terminal run cannot be continued");
    assert!(matches!(err, InvestigateRunError::AlreadyTerminal { .. }));

    let after = std::fs::read(store.state_path(&outcome.run_id).unwrap()).unwrap();
    assert_eq!(
        before, after,
        "a refused continue must leave the terminal state.json byte-identical"
    );
    let state = store.load_state(&outcome.run_id).unwrap().unwrap();
    assert_eq!(state.turn, 1, "no new turn ran on the terminal run");
    assert_no_leaked_workspace(&repo);
}

// ---------------------------------------------------------------------------
// R2 P1 regression: a corrupt started_at fails closed (terminal timeout)
// ---------------------------------------------------------------------------

/// Codex R2 P1: a corrupt/unparseable `started_at` fails CLOSED — the
/// resume terminates as `timeout` rather than being granted a fresh full
/// budget (which would let a stalled/failed run evade the wall-clock cap).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_started_at_terminates_closed() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = init_committed_repo(temp.path());
    let store = store_for(&repo);
    let fixtures = temp.path().join("fixtures");
    let silent = stage_fixture(&fixtures, "investigator-silent.sh");
    let conclude = stage_fixture(&fixtures, "investigator-conclude.sh");

    // Pause the run (stall).
    let paused = run_bounded(
        &store,
        InvestigateRunRequest::new(
            &repo,
            "why is startup slow",
            "sha-anchor",
            vec![fake_investigator(
                "inv-a",
                &silent,
                &[],
                Duration::from_secs(60),
            )],
            4,
            1,
        ),
        InvestigateCancelHandle::new(),
        120,
    )
    .await;
    assert_eq!(paused.pause_reason, Some(PauseReason::Stalled));

    // Corrupt the wall-clock anchor.
    store
        .update_state(&paused.run_id, |state| {
            state.started_at = "garbage-not-a-timestamp".to_string();
        })
        .expect("corrupt started_at");

    // A would-be-concluding resume must terminate closed (timeout).
    let resumed = continue_investigate_with_sources(
        &store,
        &paused.run_id,
        vec![fake_investigator(
            "inv-a",
            &conclude,
            &[],
            Duration::from_secs(60),
        )],
        &repo,
        DEFAULT_INVESTIGATOR_TIMEOUT,
        true,
        DEFAULT_CLAUDE_REVIEW_MAX_BUDGET_USD,
        InvestigateCancelHandle::new(),
    )
    .await
    .expect("continue completes");
    assert_eq!(
        resumed.terminal_state,
        Some(InvestigateTerminalState::Timeout),
        "a corrupt started_at must fail closed as timeout, not a fresh budget"
    );
    assert_eq!(
        resumed.turns_executed, 0,
        "no turn ran with a corrupt anchor"
    );
    assert_no_leaked_workspace(&repo);
}
