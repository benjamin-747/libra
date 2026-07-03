//! `libra deps` — the file dependency graph command family (lore.md 3.1).
//!
//! Declare and query typed, versioned per-file dependency edges. A Libra
//! extension (Git has no file-dependency-graph concept). All storage goes
//! through [`crate::internal::deps::DependencyStore`]; this module is only the
//! CLI surface.

use clap::{Parser, Subcommand};

use crate::{
    internal::deps::{DependencyStore, DepsError, Direction},
    utils::{
        error::{CliError, CliResult, StableErrorCode},
        output::{OutputConfig, emit_json_data},
        util,
    },
};

pub const DEPS_EXAMPLES: &str = "\
EXAMPLES:
    libra deps add scene.usd tex/wood.png      Declare scene.usd depends on tex/wood.png
    libra deps list scene.usd                  List the direct deps of scene.usd
    libra deps list tex/wood.png --reverse     List what depends on tex/wood.png
    libra deps tree scene.usd                  Transitive dependency closure
    libra deps why scene.usd tex/wood.png      Explain why the second is pulled in
    libra deps rm scene.usd tex/wood.png       Remove an edge

NOTE: edges are versioned per-commit under refs/notes/deps (default revision
HEAD; use --revision <rev> to target another commit). Like all refs/notes/*,
the deps ref is not auto-fetched/pushed yet — moving edges cross-machine is a
follow-up (lore.md 3.2).";

/// Manage the file dependency graph (lore.md 3.1, Libra extension).
#[derive(Parser, Debug)]
#[command(after_help = DEPS_EXAMPLES)]
pub struct DepsArgs {
    #[command(subcommand)]
    pub command: DepsCommand,
}

#[derive(Subcommand, Debug)]
pub enum DepsCommand {
    /// Declare a dependency edge `<from>` depends on `<to>`.
    Add {
        from: String,
        to: String,
        /// Edge kind label (default `generic`).
        #[clap(long, default_value = "generic")]
        kind: String,
        /// Target revision (default HEAD).
        #[clap(long, default_value = "HEAD")]
        revision: String,
    },
    /// Remove a dependency edge.
    Rm {
        from: String,
        to: String,
        /// Only remove edges of this kind (default: all kinds).
        #[clap(long)]
        kind: Option<String>,
        #[clap(long, default_value = "HEAD")]
        revision: String,
    },
    /// List direct dependencies of a path (or the whole graph if no path).
    List {
        /// The file whose neighbors to list; omit to dump every edge.
        path: Option<String>,
        /// List dependents (reverse edges) instead of dependencies.
        #[clap(long)]
        reverse: bool,
        /// Filter by edge kind.
        #[clap(long)]
        kind: Option<String>,
        #[clap(long, default_value = "HEAD")]
        revision: String,
    },
    /// Print the transitive dependency closure of a path.
    Tree {
        path: String,
        /// Traverse dependents (reverse) instead of dependencies.
        #[clap(long)]
        reverse: bool,
        /// Maximum traversal depth (unbounded if omitted).
        #[clap(long)]
        depth_limit: Option<usize>,
        #[clap(long, default_value = "HEAD")]
        revision: String,
    },
    /// Explain why `<to>` is (transitively) a dependency of `<from>`.
    Why {
        from: String,
        to: String,
        #[clap(long, default_value = "HEAD")]
        revision: String,
    },
}

fn map_err(e: DepsError) -> CliError {
    let code = match &e {
        DepsError::InvalidPath(..) => StableErrorCode::CliInvalidTarget,
        DepsError::RevisionNotFound(..) => StableErrorCode::CliInvalidTarget,
        DepsError::SelfEdge(_) => StableErrorCode::CliInvalidArguments,
        DepsError::Storage(_) => StableErrorCode::RepoStateInvalid,
    };
    CliError::fatal(e.to_string()).with_stable_code(code)
}

pub async fn execute_safe(args: DepsArgs, output: &OutputConfig) -> CliResult<()> {
    util::require_repo().map_err(|_| CliError::repo_not_found())?;
    match args.command {
        DepsCommand::Add {
            from,
            to,
            kind,
            revision,
        } => {
            DependencyStore::add_edge(&revision, &from, &to, &kind)
                .await
                .map_err(map_err)?;
            if !output.quiet {
                println!("declared {from} -> {to} ({kind})");
            }
            Ok(())
        }
        DepsCommand::Rm {
            from,
            to,
            kind,
            revision,
        } => {
            let removed =
                DependencyStore::remove_edge(&revision, &from, &to, kind.as_deref().unwrap_or(""))
                    .await
                    .map_err(map_err)?;
            if !removed {
                return Err(CliError::fatal(format!("no edge {from} -> {to} to remove"))
                    .with_stable_code(StableErrorCode::CliInvalidTarget));
            }
            if !output.quiet {
                println!("removed {from} -> {to}");
            }
            Ok(())
        }
        DepsCommand::List {
            path,
            reverse,
            kind,
            revision,
        } => {
            let direction = if reverse {
                Direction::Reverse
            } else {
                Direction::Forward
            };
            match path {
                Some(path) => {
                    let neighbors =
                        DependencyStore::direct(&revision, &path, direction, kind.as_deref())
                            .await
                            .map_err(map_err)?;
                    if output.is_json() {
                        return emit_json_data(
                            "deps.list",
                            &serde_json::json!({
                                "path": path,
                                "reverse": reverse,
                                "neighbors": neighbors,
                            }),
                            output,
                        );
                    }
                    for n in &neighbors {
                        println!("{n}");
                    }
                    if neighbors.is_empty() && !output.quiet {
                        println!(
                            "(no {})",
                            if reverse {
                                "dependents"
                            } else {
                                "dependencies"
                            }
                        );
                    }
                    Ok(())
                }
                None => {
                    let edges = DependencyStore::all_edges(&revision)
                        .await
                        .map_err(map_err)?;
                    if output.is_json() {
                        return emit_json_data(
                            "deps.list",
                            &serde_json::json!({ "edges": edges }),
                            output,
                        );
                    }
                    for e in &edges {
                        println!("{} -> {} ({})", e.from, e.to, e.kind);
                    }
                    if edges.is_empty() && !output.quiet {
                        println!("(no dependency edges)");
                    }
                    Ok(())
                }
            }
        }
        DepsCommand::Tree {
            path,
            reverse,
            depth_limit,
            revision,
        } => {
            let direction = if reverse {
                Direction::Reverse
            } else {
                Direction::Forward
            };
            let closure = DependencyStore::transitive_closure(
                &revision,
                std::slice::from_ref(&path),
                direction,
                depth_limit,
            )
            .await
            .map_err(map_err)?;
            // The closure includes the root; report the reachable set minus the
            // root as the actual dependencies.
            let deps: Vec<&String> = closure.reachable.iter().filter(|p| **p != path).collect();
            if output.is_json() {
                return emit_json_data(
                    "deps.tree",
                    &serde_json::json!({
                        "root": path,
                        "reverse": reverse,
                        "reachable": deps,
                        "cycles_detected": closure.cycles_detected,
                        "truncated": closure.truncated,
                    }),
                    output,
                );
            }
            for d in &deps {
                println!("{d}");
            }
            if closure.truncated && !output.quiet {
                eprintln!("note: traversal truncated at the depth limit");
            }
            Ok(())
        }
        DepsCommand::Why { from, to, revision } => {
            let path = DependencyStore::why(&revision, &from, &to)
                .await
                .map_err(map_err)?;
            match path {
                Some(chain) => {
                    if output.is_json() {
                        return emit_json_data(
                            "deps.why",
                            &serde_json::json!({ "reachable": true, "path": chain }),
                            output,
                        );
                    }
                    println!("{}", chain.join(" -> "));
                    Ok(())
                }
                None => {
                    if output.is_json() {
                        return emit_json_data(
                            "deps.why",
                            &serde_json::json!({ "reachable": false, "path": Vec::<String>::new() }),
                            output,
                        );
                    }
                    // Exit non-zero to distinguish "no path" for scripting.
                    Err(
                        CliError::failure(format!("'{to}' is not a dependency of '{from}'"))
                            .with_stable_code(StableErrorCode::CliInvalidTarget),
                    )
                }
            }
        }
    }
}
