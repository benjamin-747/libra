//! Integration tests for the file dependency graph (lore.md 3.1).
//!
//! Verifies: declare/query edges (direct, reverse, transitive, why); cycle-safe
//! traversal; validation (self-edge, path escape); --json; removal; and that a
//! fresh repo reads an empty graph (absence-tolerance).
//!
//! Layer: L1 (deterministic; tempdir + isolated HOME, no network).

use std::fs;

use super::{assert_cli_success, run_libra_command};

/// A committed repo with scene.usd + tex/{wood,mat}.png. Returns its dir.
fn asset_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    let p = repo.path();
    assert_cli_success(&run_libra_command(&["init"], p), "init");
    assert_cli_success(&run_libra_command(&["config", "user.name", "t"], p), "name");
    assert_cli_success(
        &run_libra_command(&["config", "user.email", "t@t"], p),
        "email",
    );
    fs::create_dir_all(p.join("tex")).unwrap();
    fs::write(p.join("scene.usd"), "scene\n").unwrap();
    fs::write(p.join("tex/wood.png"), "wood\n").unwrap();
    fs::write(p.join("tex/mat.png"), "mat\n").unwrap();
    assert_cli_success(&run_libra_command(&["add", "-A"], p), "add");
    assert_cli_success(
        &run_libra_command(&["commit", "-m", "c1", "--no-verify"], p),
        "commit",
    );
    repo
}

#[test]
fn declare_and_query_direct_reverse_transitive() {
    let repo = asset_repo();
    let p = repo.path();
    // Fresh graph is empty (absence-tolerance).
    let empty = run_libra_command(&["deps", "list", "scene.usd"], p);
    assert_cli_success(&empty, "list on empty graph");

    // scene -> wood -> mat, scene -> mat.
    for (f, t) in [
        ("scene.usd", "tex/wood.png"),
        ("tex/wood.png", "tex/mat.png"),
        ("scene.usd", "tex/mat.png"),
    ] {
        assert_cli_success(&run_libra_command(&["deps", "add", f, t], p), "add edge");
    }

    // Direct deps of scene.
    let direct = run_libra_command(&["deps", "list", "scene.usd"], p);
    let out = String::from_utf8_lossy(&direct.stdout);
    assert!(
        out.contains("tex/wood.png") && out.contains("tex/mat.png"),
        "direct: {out}"
    );

    // Reverse: what depends on mat → scene + wood.
    let rev = run_libra_command(&["deps", "list", "tex/mat.png", "--reverse"], p);
    let rout = String::from_utf8_lossy(&rev.stdout);
    assert!(
        rout.contains("scene.usd") && rout.contains("tex/wood.png"),
        "reverse: {rout}"
    );

    // Transitive closure of scene → {wood, mat}.
    let tree = run_libra_command(&["deps", "tree", "scene.usd"], p);
    let tout = String::from_utf8_lossy(&tree.stdout);
    assert!(
        tout.contains("tex/wood.png") && tout.contains("tex/mat.png"),
        "tree: {tout}"
    );

    // why scene -> mat is reachable.
    let why = run_libra_command(&["deps", "why", "scene.usd", "tex/mat.png"], p);
    assert_cli_success(&why, "why reachable");
    assert!(String::from_utf8_lossy(&why.stdout).contains("scene.usd"));

    // why for an unrelated pair is non-zero.
    let no = run_libra_command(&["deps", "why", "tex/mat.png", "scene.usd"], p);
    assert_ne!(no.status.code(), Some(0), "unreachable why exits non-zero");
}

#[test]
fn transitive_closure_is_cycle_safe() {
    let repo = asset_repo();
    let p = repo.path();
    // A cycle: scene -> wood -> mat -> scene.
    for (f, t) in [
        ("scene.usd", "tex/wood.png"),
        ("tex/wood.png", "tex/mat.png"),
        ("tex/mat.png", "scene.usd"),
    ] {
        assert_cli_success(&run_libra_command(&["deps", "add", f, t], p), "add");
    }
    // tree must terminate (not hang) and list the other two nodes.
    let tree = run_libra_command(&["deps", "tree", "scene.usd"], p);
    assert_cli_success(&tree, "cycle tree terminates");
    let out = String::from_utf8_lossy(&tree.stdout);
    assert!(
        out.contains("tex/wood.png") && out.contains("tex/mat.png"),
        "{out}"
    );
    // JSON marks cycles_detected.
    let js_out = run_libra_command(&["--json", "deps", "tree", "scene.usd"], p);
    let js: serde_json::Value = serde_json::from_slice(&js_out.stdout).unwrap();
    assert_eq!(js["data"]["cycles_detected"].as_bool(), Some(true));
}

#[test]
fn validation_rejects_self_edge_and_escape() {
    let repo = asset_repo();
    let p = repo.path();
    let self_edge = run_libra_command(&["deps", "add", "scene.usd", "scene.usd"], p);
    assert_ne!(self_edge.status.code(), Some(0), "self-edge rejected");
    let escape = run_libra_command(&["deps", "add", "scene.usd", "../x"], p);
    assert_ne!(escape.status.code(), Some(0), "path escape rejected");
    let abs = run_libra_command(&["deps", "add", "scene.usd", "/etc/passwd"], p);
    assert_ne!(abs.status.code(), Some(0), "absolute path rejected");
}

#[test]
fn add_is_idempotent_and_rm_clears() {
    let repo = asset_repo();
    let p = repo.path();
    assert_cli_success(
        &run_libra_command(&["deps", "add", "scene.usd", "tex/wood.png"], p),
        "add",
    );
    // Duplicate add is a no-op (no error, no dup).
    assert_cli_success(
        &run_libra_command(&["deps", "add", "scene.usd", "tex/wood.png"], p),
        "dup add",
    );
    let js_out = run_libra_command(&["--json", "deps", "list", "scene.usd"], p);
    let js: serde_json::Value = serde_json::from_slice(&js_out.stdout).unwrap();
    assert_eq!(
        js["data"]["neighbors"].as_array().map(|a| a.len()),
        Some(1),
        "no duplicate edge"
    );

    // Removing the only edge leaves an empty graph (note removed).
    assert_cli_success(
        &run_libra_command(&["deps", "rm", "scene.usd", "tex/wood.png"], p),
        "rm",
    );
    let after = run_libra_command(&["--json", "deps", "list", "scene.usd"], p);
    let js2: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    assert_eq!(
        js2["data"]["neighbors"].as_array().map(|a| a.len()),
        Some(0)
    );
    // Removing an absent edge is an error (non-zero).
    let gone = run_libra_command(&["deps", "rm", "scene.usd", "tex/wood.png"], p);
    assert_ne!(
        gone.status.code(),
        Some(0),
        "removing an absent edge errors"
    );
}
