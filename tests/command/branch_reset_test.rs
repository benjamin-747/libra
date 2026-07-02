//! Integration tests for `libra branch reset` (lore.md §1.13): SQLite ref +
//! reflog update, worktree/index untouched, protect/archive enforced
//! fail-closed (the first policy-layer consumer), update-ref covered too.
//!
//! **Layer:** L1 — deterministic.

use super::*;

fn reset_repo() -> tempfile::TempDir {
    let repo = create_committed_repo_via_cli();
    let p = repo.path();
    fs::write(p.join("f.txt"), "a\n").unwrap();
    assert_cli_success(&run_libra_command(&["add", "f.txt"], p), "add");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "c1", "--no-verify"], p),
        "c1",
    );
    assert_cli_success(&run_libra_command(&["branch", "feature"], p), "branch");
    fs::write(p.join("f.txt"), "b\n").unwrap();
    assert_cli_success(&run_libra_command(&["add", "f.txt"], p), "add2");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "c2", "--no-verify"], p),
        "c2",
    );
    repo
}

#[test]
fn branch_reset_moves_tip_without_touching_worktree() {
    let repo = reset_repo();
    let p = repo.path();
    let index_before = fs::read(p.join(".libra/index")).unwrap();
    let file_before = fs::read_to_string(p.join("f.txt")).unwrap();
    let out = run_libra_command(&["--json", "branch", "reset", "feature", "HEAD"], p);
    assert_cli_success(&out, "reset");
    let json = parse_json_stdout(&out);
    assert_eq!(json["data"]["action"].as_str(), Some("reset"));
    assert_ne!(
        json["data"]["old_commit"].as_str(),
        json["data"]["new_commit"].as_str(),
        "tip moved: {json}"
    );
    // Index bytes and worktree untouched.
    assert_eq!(fs::read(p.join(".libra/index")).unwrap(), index_before);
    assert_eq!(fs::read_to_string(p.join("f.txt")).unwrap(), file_before);
    // An IDENTICAL re-run inside the operation-log's 5s dedup window is
    // refused (documented; the digest covers reset/branch/new-commit) —
    // outside the window it succeeds. Pin the refusal shape.
    let again = run_libra_command(&["branch", "reset", "feature", "HEAD"], p);
    assert_eq!(again.status.code(), Some(128), "5s dedup window refusal");
    assert!(
        String::from_utf8_lossy(&again.stderr).contains("duplicate operation"),
        "{}",
        String::from_utf8_lossy(&again.stderr)
    );
    // A reflog entry exists for the branch.
    let reflog = run_libra_command(&["reflog", "show", "refs/heads/feature"], p);
    if reflog.status.success() {
        assert!(
            String::from_utf8_lossy(&reflog.stdout).contains("reset"),
            "reflog records the move: {}",
            String::from_utf8_lossy(&reflog.stdout)
        );
    }
}

#[test]
fn branch_reset_policy_enforcement_matrix() {
    let repo = reset_repo();
    let p = repo.path();
    // protect blocks (fail-closed truthy), LBR-POLICY-001, exit 128.
    assert_cli_success(
        &run_libra_command(
            &["metadata", "set", "protect", "true", "--branch", "feature"],
            p,
        ),
        "protect",
    );
    let refused = run_libra_command(&["branch", "reset", "feature", "HEAD"], p);
    assert_eq!(refused.status.code(), Some(128));
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(
        err.contains("LBR-POLICY-001") && err.contains("protected"),
        "{err}"
    );
    assert!(err.contains("metadata unset"), "lift-hint present: {err}");
    // GARBAGE value is fail-closed too (still protected).
    assert_cli_success(
        &run_libra_command(
            &[
                "metadata", "set", "protect", "banana", "--branch", "feature",
            ],
            p,
        ),
        "garbage protect",
    );
    let refused = run_libra_command(&["branch", "reset", "feature", "HEAD"], p);
    assert_eq!(
        refused.status.code(),
        Some(128),
        "garbage value fails closed"
    );
    // unset → allowed.
    assert_cli_success(
        &run_libra_command(&["metadata", "unset", "protect", "--branch", "feature"], p),
        "unset",
    );
    assert_cli_success(
        &run_libra_command(&["branch", "reset", "feature", "HEAD"], p),
        "after unset",
    );
    // archive blocks identically.
    assert_cli_success(
        &run_libra_command(
            &["metadata", "set", "archive", "true", "--branch", "feature"],
            p,
        ),
        "archive",
    );
    let refused = run_libra_command(&["branch", "reset", "feature", "HEAD"], p);
    assert_eq!(refused.status.code(), Some(128));
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("archived"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    // update-ref enforces the same policy (no plumbing bypass) — update AND delete.
    let oid_out = run_libra_command(&["--json", "log", "-n", "1"], p);
    let oid = parse_json_stdout(&oid_out)["data"]["commits"][0]["hash"]
        .as_str()
        .unwrap()
        .to_string();
    let blocked = run_libra_command(&["update-ref", "refs/heads/feature", &oid], p);
    assert_eq!(blocked.status.code(), Some(128), "update-ref blocked");
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("LBR-POLICY-001"),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    let blocked = run_libra_command(&["update-ref", "-d", "refs/heads/feature"], p);
    assert_eq!(blocked.status.code(), Some(128), "update-ref -d blocked");
}

#[test]
fn branch_reset_refusals_and_reserved_verb() {
    let repo = reset_repo();
    let p = repo.path();
    // Current branch refused with the reset hint.
    let cur = run_libra_command(&["branch", "reset", "main", "HEAD~1"], p);
    assert_eq!(cur.status.code(), Some(128));
    assert!(
        String::from_utf8_lossy(&cur.stderr).contains("currently checked out"),
        "{}",
        String::from_utf8_lossy(&cur.stderr)
    );
    // Unknown branch → suggestions; unknown target → invalid target.
    let nf = run_libra_command(&["branch", "reset", "faeture", "HEAD"], p);
    assert_eq!(nf.status.code(), Some(129));
    let bad = run_libra_command(&["branch", "reset", "feature", "deadbeef"], p);
    assert_eq!(bad.status.code(), Some(129));
    // Reserved verb: flags + 'reset' refuse (never creates a branch 'reset').
    let out = run_libra_command(&["branch", "-v", "reset"], p);
    assert_eq!(out.status.code(), Some(129));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("reserved branch verb"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = run_libra_command(&["branch", "--list"], p);
    assert!(
        !String::from_utf8_lossy(&list.stdout).contains("reset"),
        "{}",
        String::from_utf8_lossy(&list.stdout)
    );
}
