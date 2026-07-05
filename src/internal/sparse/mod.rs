//! Read-only sparse VIEW filter (lore.md 2.2) — the non-declined complement of
//! git sparse-checkout (D10 defers the materializing form). A stored allowlist
//! of gitignore-syntax include patterns scopes WHAT the read/query commands
//! (`ls-files`, `diff`) OUTPUT. It NEVER mutates the working tree, never writes
//! skip-worktree bits, and — critically — never filters the changes-to-be-
//! committed set that `commit` records, so `status`'s dirtiness and exit code
//! stay HONEST (a sparse view must not make status lie about what commit will
//! do). `status` only surfaces a one-line advisory that a view is active.
//!
//! State: the ordered pattern list lives in the `sparse_view` table (owned
//! solely by [`SparseViewStore`], §3.6); the `sparse.enabled` toggle lives in
//! `config_kv` (mirroring git's `core.sparseCheckout` split). Absence-tolerant:
//! a missing table (pre-migration / old binary) resolves to an empty view.

use std::path::Path;

use ignore::{Match, gitignore::Gitignore};
use sea_orm::{ConnectionTrait, DbBackend, Statement, TransactionTrait};

use crate::{
    internal::{config::ConfigKv, db::get_db_conn_instance},
    utils::util,
};

const ENABLED_KEY: &str = "sparse.enabled";

/// Single-owner store over `sparse_view` + the `sparse.enabled` config toggle.
pub struct SparseViewStore;

impl SparseViewStore {
    /// The ordered include patterns (empty if the table is absent).
    pub async fn list() -> Result<Vec<String>, String> {
        let db = get_db_conn_instance().await;
        let stmt = Statement::from_string(
            DbBackend::Sqlite,
            "SELECT pattern FROM sparse_view ORDER BY ordinal ASC, id ASC".to_string(),
        );
        let rows = match db.query_all(stmt).await {
            Ok(rows) => rows,
            Err(e) if e.to_string().contains("no such table") => return Ok(Vec::new()),
            Err(e) => return Err(format!("failed to list the sparse view: {e}")),
        };
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(row.try_get_by_index(0).map_err(|e| e.to_string())?);
        }
        Ok(out)
    }

    /// Whether the view is enabled (config toggle). Default false.
    pub async fn is_enabled() -> bool {
        ConfigKv::get(ENABLED_KEY)
            .await
            .ok()
            .flatten()
            .map(|entry| matches!(entry.value.trim(), "true" | "1" | "yes" | "on"))
            .unwrap_or(false)
    }

    async fn set_enabled(enabled: bool) -> Result<(), String> {
        let db = get_db_conn_instance().await;
        ConfigKv::set_with_conn(
            &db,
            ENABLED_KEY,
            if enabled { "true" } else { "false" },
            false,
        )
        .await
        .map_err(|e| format!("failed to set {ENABLED_KEY}: {e}"))
    }

    /// Replace the whole pattern list (transactional) and ENABLE the view.
    pub async fn replace(patterns: &[String]) -> Result<(), String> {
        Self::rewrite(patterns).await?;
        Self::set_enabled(true).await
    }

    /// Append patterns (keeping order) and ENABLE the view.
    pub async fn add(patterns: &[String]) -> Result<(), String> {
        let mut all = Self::list().await?;
        all.extend(patterns.iter().cloned());
        Self::rewrite(&all).await?;
        Self::set_enabled(true).await
    }

    /// Drop every pattern and DISABLE the view.
    pub async fn clear() -> Result<(), String> {
        Self::rewrite(&[]).await?;
        Self::set_enabled(false).await
    }

    /// Enable / disable without changing the patterns.
    pub async fn enable() -> Result<(), String> {
        Self::set_enabled(true).await
    }
    pub async fn disable() -> Result<(), String> {
        Self::set_enabled(false).await
    }

    async fn rewrite(patterns: &[String]) -> Result<(), String> {
        let db = get_db_conn_instance().await;
        let txn = db.begin().await.map_err(|e| e.to_string())?;
        txn.execute(Statement::from_string(
            DbBackend::Sqlite,
            "DELETE FROM sparse_view".to_string(),
        ))
        .await
        .map_err(|e| format!("failed to clear the sparse view: {e}"))?;
        for (ordinal, pattern) in patterns.iter().enumerate() {
            txn.execute(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO sparse_view (pattern, ordinal) VALUES (?, ?)",
                [pattern.as_str().into(), (ordinal as i64).into()],
            ))
            .await
            .map_err(|e| format!("failed to record a sparse pattern: {e}"))?;
        }
        txn.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }
}

/// A compiled sparse view ready for per-path verdicts. `None` when the view is
/// disabled or has no patterns — in which case EVERYTHING is in view (a
/// deliberate anti-footgun: an enabled-but-empty view degrades to a no-op
/// rather than hiding the whole tree).
pub struct SparseView {
    matcher: Option<Gitignore>,
    workdir: std::path::PathBuf,
}

impl SparseView {
    /// Load + compile the active view. Returns a no-op view (`is_active()` ==
    /// false) when disabled/empty or on any load error (read-only filters must
    /// never fail a query command).
    pub async fn load() -> Self {
        let workdir = util::working_dir();
        if !SparseViewStore::is_enabled().await {
            return Self {
                matcher: None,
                workdir,
            };
        }
        let patterns = SparseViewStore::list().await.unwrap_or_default();
        let matcher = util::build_exclude_matcher(&workdir, &patterns)
            .ok()
            .flatten();
        Self { matcher, workdir }
    }

    /// Whether the view actually filters anything.
    pub fn is_active(&self) -> bool {
        self.matcher.is_some()
    }

    /// Is `rel_path` (repo-root-relative, either separator) IN the view? Always
    /// true for a no-op view. ALLOWLIST semantics (lore.md 2.2 / Codex MF2):
    /// the last matching pattern wins — an `Ignore` verdict means in-view, a
    /// `Whitelist` (`!pat`) means the path was carved back OUT even under a
    /// broader include, and `None` (no pattern matched) means out-of-view
    /// (a view is an allowlist). NO ancestor-dominance short-circuit (that is
    /// exclude semantics and would defeat `!child` negations).
    pub fn contains(&self, rel_path: &Path) -> bool {
        let Some(matcher) = &self.matcher else {
            return true;
        };
        let abs = self.workdir.join(rel_path);
        match matcher.matched(&abs, false) {
            Match::Ignore(_) => true,
            Match::Whitelist(_) | Match::None => false,
        }
    }

    /// Convenience for string paths.
    pub fn contains_str(&self, rel_path: &str) -> bool {
        self.contains(Path::new(rel_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::test::{ChangeDirGuard, setup_with_new_libra_in};

    /// Allowlist verdict (Codex MF2): last-match-wins, `!child` re-excludes,
    /// no ancestor-dominance short-circuit, default-exclude.
    #[test]
    fn allowlist_verdict_honors_negation() {
        let dir = tempfile::tempdir().expect("tmp");
        let workdir = dir.path().to_path_buf();
        let matcher = util::build_exclude_matcher(
            &workdir,
            &["src/**".to_string(), "!src/gen/**".to_string()],
        )
        .expect("compile")
        .expect("some");
        let view = SparseView {
            matcher: Some(matcher),
            workdir,
        };
        assert!(view.contains_str("src/a.txt"), "included by src/**");
        assert!(
            !view.contains_str("src/gen/g.txt"),
            "!src/gen/** carves it OUT"
        );
        assert!(
            !view.contains_str("docs/d.txt"),
            "default-exclude (allowlist)"
        );
    }

    /// A no-op view (disabled/empty) includes everything.
    #[test]
    fn noop_view_includes_all() {
        let view = SparseView {
            matcher: None,
            workdir: std::path::PathBuf::from("/x"),
        };
        assert!(!view.is_active());
        assert!(view.contains_str("anything/at/all.txt"));
    }

    /// Store round-trip: set/add/list ordering + enable/disable/clear.
    #[tokio::test]
    #[serial_test::serial]
    async fn store_round_trip() {
        let tmp = tempfile::tempdir().expect("tmp");
        let _guard = ChangeDirGuard::new(tmp.path());
        setup_with_new_libra_in(tmp.path()).await;

        assert!(!SparseViewStore::is_enabled().await);
        SparseViewStore::replace(&["a/**".to_string(), "b/**".to_string()])
            .await
            .expect("replace");
        assert!(SparseViewStore::is_enabled().await, "replace enables");
        assert_eq!(
            SparseViewStore::list().await.expect("list"),
            vec!["a/**", "b/**"]
        );
        SparseViewStore::add(&["!a/x/**".to_string()])
            .await
            .expect("add");
        assert_eq!(
            SparseViewStore::list().await.expect("list"),
            vec!["a/**", "b/**", "!a/x/**"]
        );
        SparseViewStore::disable().await.expect("disable");
        assert!(!SparseViewStore::is_enabled().await);
        assert_eq!(
            SparseViewStore::list().await.expect("list").len(),
            3,
            "patterns kept"
        );
        SparseViewStore::clear().await.expect("clear");
        assert!(SparseViewStore::list().await.expect("list").is_empty());
        assert!(!SparseViewStore::is_enabled().await);
    }
}
