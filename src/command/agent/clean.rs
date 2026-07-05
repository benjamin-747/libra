//! `libra agent clean [--all]` — drop temporary checkpoints from stopped
//! sessions per `docs/development/commands/_general.md` §7.4.
//!
//! The default form scopes cleanup to the most recently stopped session;
//! `--all` widens that to every stopped session. Active sessions are never
//! cleaned because a temporary checkpoint may still be part of an in-flight
//! external-agent turn.
//!
//! When checkpoint commits are present, cleanup rewrites
//! `refs/libra/traces` so pruned temporary checkpoints stop being
//! reachable. Older DB-only fixtures with an empty ref still get the catalog
//! cleanup without a rewrite.
//!
//! AG-20 prune safety: the underlying
//! [`HistoryManager::prune_checkpoint_commits`] fails closed while a
//! checkpoint write is in flight (live in-flight marker, window A/B) or when
//! `refs/libra/traces` reaches commits missing from the checkpoint catalog
//! (window-B residue — `libra agent doctor --repair` territory). It also
//! emits the `agent.clean.prune` span (deleted_objects / deleted_sessions /
//! window_guard / duration_ms) and drops `object_index` rows for OIDs the
//! prune made unreachable.

use std::sync::Arc;

use sea_orm::{ConnectionTrait, Statement};
use serde::Serialize;

use super::CleanArgs;
use crate::{
    internal::{
        ai::history::{CheckpointPruneGuardError, HistoryManager},
        branch::TRACES_BRANCH,
        db::get_db_conn_instance,
    },
    utils::{
        client_storage::ClientStorage,
        error::{CliError, CliResult},
        output::{OutputConfig, emit_json_data},
        util,
    },
};

#[derive(Debug, Serialize)]
struct CleanReport {
    sessions_inspected: i64,
    temporary_checkpoints_dropped: u64,
    retained_checkpoints_rewritten: usize,
    traces_ref_rewritten: bool,
    /// Which AG-20 window-guard path the prune took (`noop` /
    /// `markers_and_catalog_verified`).
    window_guard: &'static str,
    /// `object_index` rows dropped for OIDs the prune made unreachable.
    object_index_rows_dropped: u64,
    note: &'static str,
}

pub async fn execute_safe(args: CleanArgs, output: &OutputConfig) -> CliResult<()> {
    let conn = get_db_conn_instance().await;
    let backend = conn.get_database_backend();

    if !table_exists(&conn, "agent_checkpoint").await? {
        return emit_report(
            &CleanReport {
                sessions_inspected: 0,
                temporary_checkpoints_dropped: 0,
                retained_checkpoints_rewritten: 0,
                traces_ref_rewritten: false,
                window_guard: "noop",
                object_index_rows_dropped: 0,
                note: "agent_checkpoint table not present (run `libra init`?)",
            },
            output,
        );
    }

    // Retention GC (AG-24a) always spans every stopped session; the
    // default/temporary path keeps the `--all` scoping.
    let session_scope = session_scope_sql(args.all || args.gc);
    let session_filter = format!("SELECT COUNT(*) AS n FROM ({session_scope}) AS scoped_sessions");
    let row = conn
        .query_one(Statement::from_string(backend, session_filter))
        .await
        .map_err(|e| CliError::fatal(format!("failed to count agent_session: {e}")))?
        .ok_or_else(|| CliError::fatal("agent_session count returned no rows".to_string()))?;
    let sessions_inspected: i64 = row.try_get_by("n").unwrap_or_default();

    let (checkpoint_ids, note): (Vec<String>, &'static str) = if args.gc {
        // Resolve the transcript retention window (default 90). The cutoff
        // is `created_at < now - window`. GC removes whole aged checkpoints
        // (transcript + stderr blobs) — the append-only `agent_audit_log`
        // is a separate table the prune engine never touches.
        let retention_days = match args.retention_days {
            Some(0) => {
                return Err(CliError::command_usage(
                    "--retention-days must be greater than 0".to_string(),
                ));
            }
            Some(days) => days,
            None => crate::internal::ai::observed_agents::compliance::retention_transcript_days()
                .await
                .map_err(|e| CliError::fatal(format!("read retention config: {e:#}")))?,
        };
        let cutoff_unix = chrono::Utc::now().timestamp() - i64::from(retention_days) * 86_400;
        (
            gc_expired_checkpoint_ids(&conn, session_scope, cutoff_unix).await?,
            "retention GC dropped checkpoints older than agent.retention.transcript_days from \
             stopped sessions; the append-only agent_audit_log was not touched",
        )
    } else {
        (
            temporary_checkpoint_ids(&conn, session_scope).await?,
            "temporary checkpoint rows were dropped; reachable traces history was \
             rewritten when checkpoint commits existed",
        )
    };
    let repo_path = util::try_get_storage_path(None)
        .map_err(|e| CliError::fatal(format!("failed to locate .libra directory: {e}")))?;
    let storage = Arc::new(ClientStorage::init(repo_path.join("objects")));
    let history =
        HistoryManager::new_with_ref(storage, repo_path, Arc::new(conn.clone()), TRACES_BRANCH);
    let prune = history
        .prune_checkpoint_commits(&checkpoint_ids)
        .await
        .map_err(map_prune_error)?;

    emit_report(
        &CleanReport {
            sessions_inspected,
            temporary_checkpoints_dropped: prune.removed_checkpoints,
            retained_checkpoints_rewritten: prune.rewritten_checkpoints,
            traces_ref_rewritten: prune.ref_rewritten,
            window_guard: prune.window_guard,
            object_index_rows_dropped: prune.deleted_object_index_rows,
            note,
        },
        output,
    )
}

/// Map a prune failure to an actionable CLI error, keeping the AG-20
/// window-guard refusals distinguishable (they are deterministic and
/// user-resolvable, not storage corruption).
fn map_prune_error(err: anyhow::Error) -> CliError {
    match err.downcast_ref::<CheckpointPruneGuardError>() {
        Some(CheckpointPruneGuardError::LiveWriterMarker { .. }) => {
            CliError::conflict(format!("{err}"))
                .with_hint("an external-agent checkpoint write is still in flight")
                .with_hint(
                    "retry once the write completes; a crashed writer's marker expires \
                     automatically after its TTL",
                )
        }
        Some(CheckpointPruneGuardError::RefCatalogOrphans { .. }) => {
            CliError::conflict(format!("{err}"))
                .with_hint("run 'libra agent doctor --repair' to backfill the checkpoint catalog")
                .with_hint("then re-run 'libra agent clean'")
        }
        None => CliError::fatal(format!("failed to prune traces checkpoints: {err:#}")),
    }
}

fn session_scope_sql(all: bool) -> &'static str {
    if all {
        return "SELECT session_id FROM agent_session WHERE state = 'stopped'";
    }
    "SELECT session_id FROM agent_session \
     WHERE state = 'stopped' \
     ORDER BY COALESCE(stopped_at, last_event_at, started_at) DESC, session_id DESC \
     LIMIT 1"
}

fn emit_report(report: &CleanReport, output: &OutputConfig) -> CliResult<()> {
    if output.is_json() {
        return emit_json_data("agent_clean", report, output);
    }
    if output.quiet {
        return Ok(());
    }
    println!(
        "Sessions inspected            : {}",
        report.sessions_inspected
    );
    println!(
        "Temporary checkpoints dropped : {}",
        report.temporary_checkpoints_dropped
    );
    println!(
        "Object index rows dropped     : {}",
        report.object_index_rows_dropped
    );
    println!("Note                          : {}", report.note);
    Ok(())
}

async fn table_exists(conn: &(impl ConnectionTrait + ?Sized), name: &str) -> CliResult<bool> {
    let backend = conn.get_database_backend();
    let stmt = Statement::from_sql_and_values(
        backend,
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ? LIMIT 1",
        [name.into()],
    );
    conn.query_one(stmt)
        .await
        .map(|row| row.is_some())
        .map_err(|e| CliError::fatal(format!("failed to query sqlite_master: {e}")))
}

/// Checkpoint ids from the scoped (stopped) sessions whose `created_at`
/// predates the retention cutoff — the AG-24a retention GC selection. All
/// scopes are eligible (a 90-day-old committed checkpoint is as expired as
/// a temporary one); the append-only `agent_audit_log` lives in a separate
/// table and is never named here.
async fn gc_expired_checkpoint_ids(
    conn: &(impl ConnectionTrait + ?Sized),
    session_scope: &str,
    cutoff_unix: i64,
) -> CliResult<Vec<String>> {
    let backend = conn.get_database_backend();
    let query = format!(
        "SELECT checkpoint_id FROM agent_checkpoint \
         WHERE created_at < ? \
         AND session_id IN (SELECT session_id FROM ({session_scope}) AS scoped_sessions) \
         ORDER BY created_at ASC, checkpoint_id ASC"
    );
    let rows = conn
        .query_all(Statement::from_sql_and_values(
            backend,
            &query,
            [cutoff_unix.into()],
        ))
        .await
        .map_err(|e| CliError::fatal(format!("failed to list expired checkpoints: {e}")))?;
    rows.into_iter()
        .map(|row| {
            row.try_get_by("checkpoint_id")
                .map_err(|e| CliError::fatal(format!("failed to decode checkpoint_id: {e}")))
        })
        .collect()
}

async fn temporary_checkpoint_ids(
    conn: &(impl ConnectionTrait + ?Sized),
    session_scope: &str,
) -> CliResult<Vec<String>> {
    let backend = conn.get_database_backend();
    let query = format!(
        "SELECT checkpoint_id FROM agent_checkpoint WHERE scope = 'temporary' \
         AND session_id IN (SELECT session_id FROM ({session_scope}) AS scoped_sessions) \
         ORDER BY created_at ASC, checkpoint_id ASC"
    );
    let rows = conn
        .query_all(Statement::from_string(backend, query))
        .await
        .map_err(|e| CliError::fatal(format!("failed to list temporary checkpoints: {e}")))?;
    rows.into_iter()
        .map(|row| {
            row.try_get_by("checkpoint_id")
                .map_err(|e| CliError::fatal(format!("failed to decode checkpoint_id: {e}")))
        })
        .collect()
}
