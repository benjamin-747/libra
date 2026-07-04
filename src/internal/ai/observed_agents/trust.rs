//! Trust / provenance store for external `libra-agent-*` binaries (AG-18).
//!
//! External binaries are quarantined by default: discovery never registers
//! them as callable, and `rpc invoke` refuses until the operator records a
//! trust entry with `libra agent rpc trust <slug>`. A trust record pins the
//! binary's canonical path plus provenance markers (sha256, device, inode,
//! mtime); every subsequent invoke revalidates them and any drift revokes
//! the record fail-closed (`LBR-AGENT-005`).
//!
//! TOCTOU note: Rust's `std::process::Command` cannot portably exec from an
//! already-verified file descriptor, so this module implements the
//! best-effort mitigation tier from `docs/development/tracing/agent.md`
//! (威胁 T9 / 强制补强项 #2): canonical absolute path, parent directory not
//! world-writable, sha256 + device/inode/mtime revalidation immediately
//! before spawn, and quarantine on any mismatch. This narrows but does not
//! eliminate the check-to-exec race; fd-derived exec is future work.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::internal::config::ConfigKv;

/// Config key prefix for trust records (one JSON value per slug).
const TRUST_KEY_PREFIX: &str = "agent.trust.";

/// Settings gate for the whole external-agent surface (E2). While this is
/// off (the default) every `agent rpc` entry point that touches external
/// binaries — `list` discovery, `trust`, `invoke` — refuses with
/// `LBR-AGENT-002`; only `untrust` (which strictly tightens security)
/// stays available. Key spelling follows the settings table in
/// `docs/development/tracing/agent.md`.
pub const EXTERNAL_AGENTS_ENABLED_KEY: &str = "agent.external_agents.enabled";

/// One recorded trust decision for an external binary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrustRecord {
    pub path: PathBuf,
    /// Lowercase hex sha256 of the binary contents.
    pub sha256: String,
    pub device: u64,
    pub inode: u64,
    /// mtime as unix seconds (best-effort; 0 when unavailable).
    pub mtime: i64,
}

/// Provenance markers computed from the binary on disk.
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    pub canonical_path: PathBuf,
    pub sha256: String,
    pub device: u64,
    pub inode: u64,
    pub mtime: i64,
}

/// Whether the operator has opted in to external `libra-agent-*` agents.
/// Absent or non-true values mean disabled (fail-closed default).
pub async fn external_agents_enabled() -> Result<bool> {
    let entry = ConfigKv::get(EXTERNAL_AGENTS_ENABLED_KEY)
        .await
        .context("read agent.external_agents.enabled")?;
    Ok(entry
        .map(|e| {
            let v = e.value.trim().to_ascii_lowercase();
            v == "true" || v == "1" || v == "yes" || v == "on"
        })
        .unwrap_or(false))
}

/// Compute the provenance markers for `path` (canonicalizes first).
pub fn compute_provenance(path: &Path) -> Result<Provenance> {
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("canonicalize external agent binary {}", path.display()))?;
    let bytes = std::fs::read(&canonical_path)
        .with_context(|| format!("read external agent binary {}", canonical_path.display()))?;
    let sha256 = hex::encode(Sha256::digest(&bytes));
    let meta = std::fs::metadata(&canonical_path)
        .with_context(|| format!("stat external agent binary {}", canonical_path.display()))?;
    #[cfg(unix)]
    let (device, inode, mtime) = {
        use std::os::unix::fs::MetadataExt;
        (meta.dev(), meta.ino(), meta.mtime())
    };
    #[cfg(not(unix))]
    let (device, inode, mtime) = {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (0u64, 0u64, mtime)
    };
    Ok(Provenance {
        canonical_path,
        sha256,
        device,
        inode,
        mtime,
    })
}

/// Best-effort spawn-surface hardening: the binary's parent directory must
/// not be world-writable (a world-writable dir lets any local user swap the
/// verified binary between check and exec).
pub fn ensure_parent_not_world_writable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("binary {} has no parent directory", path.display()))?;
        let meta = std::fs::metadata(parent)
            .with_context(|| format!("stat parent directory {}", parent.display()))?;
        if meta.permissions().mode() & 0o002 != 0 {
            bail!(
                "parent directory {} of the external agent binary is world-writable; \
                 refusing to spawn (move the binary to a protected directory)",
                parent.display()
            );
        }
    }
    Ok(())
}

fn trust_key(slug: &str) -> String {
    format!("{TRUST_KEY_PREFIX}{slug}")
}

/// Record trust for `slug` at `path`, replacing any previous record.
///
/// Fails closed (nothing is persisted) when the binary's parent directory
/// is world-writable: trusting such a binary would be meaningless because
/// any local user could swap it before the invoke-time revalidation.
pub async fn record_trust(slug: &str, path: &Path) -> Result<TrustRecord> {
    let provenance = compute_provenance(path)?;
    ensure_parent_not_world_writable(&provenance.canonical_path)?;
    let record = TrustRecord {
        path: provenance.canonical_path.clone(),
        sha256: provenance.sha256,
        device: provenance.device,
        inode: provenance.inode,
        mtime: provenance.mtime,
    };
    let value = serde_json::to_string(&record).context("serialize trust record")?;
    ConfigKv::set(&trust_key(slug), &value, false)
        .await
        .with_context(|| format!("persist trust record for '{slug}'"))?;
    Ok(record)
}

/// Read the trust record for `slug`, if any.
pub async fn read_trust(slug: &str) -> Result<Option<TrustRecord>> {
    let Some(entry) = ConfigKv::get(&trust_key(slug))
        .await
        .with_context(|| format!("read trust record for '{slug}'"))?
    else {
        return Ok(None);
    };
    let record: TrustRecord = serde_json::from_str(&entry.value)
        .with_context(|| format!("parse trust record for '{slug}' (corrupt config value)"))?;
    Ok(Some(record))
}

/// Remove the trust record for `slug`. Returns whether one existed.
pub async fn revoke_trust(slug: &str) -> Result<bool> {
    let removed = ConfigKv::unset(&trust_key(slug))
        .await
        .with_context(|| format!("remove trust record for '{slug}'"))?;
    Ok(removed > 0)
}

/// Pure drift check between freshly computed provenance and a recorded
/// trust decision: any single marker changing (hash, device, inode,
/// mtime or canonical path) counts as drift.
pub fn provenance_drifted(provenance: &Provenance, record: &TrustRecord) -> bool {
    provenance.sha256 != record.sha256
        || provenance.device != record.device
        || provenance.inode != record.inode
        || provenance.mtime != record.mtime
        || provenance.canonical_path != record.path
}

/// Revalidate the recorded trust for `slug` against the binary on disk,
/// immediately before spawn. Any drift (hash, device, inode, mtime or path)
/// revokes the record and fails closed — the binary returns to quarantine
/// until the operator re-trusts it (E2 / `LBR-AGENT-005`).
pub async fn revalidate_trust(slug: &str, record: &TrustRecord) -> Result<Provenance> {
    let provenance = match compute_provenance(&record.path) {
        Ok(p) => p,
        Err(err) => {
            let _ = revoke_trust(slug).await;
            return Err(err.context(format!(
                "trusted binary for '{slug}' is no longer readable; trust revoked"
            )));
        }
    };
    if provenance_drifted(&provenance, record) {
        let _ = revoke_trust(slug).await;
        bail!(
            "external agent binary for '{slug}' changed since it was trusted \
             (sha256/device/inode/mtime drift); trust revoked — re-run \
             'libra agent rpc trust {slug}' after verifying the binary"
        );
    }
    Ok(provenance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_provenance_hashes_and_stats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libra-agent-demo");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let p = compute_provenance(&path).unwrap();
        assert_eq!(p.sha256.len(), 64);
        assert!(p.canonical_path.is_absolute());
        #[cfg(unix)]
        {
            assert_ne!(p.inode, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn world_writable_parent_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libra-agent-demo");
        std::fs::write(&path, b"x").unwrap();
        ensure_parent_not_world_writable(&path).expect("0700 tempdir parent is fine");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = ensure_parent_not_world_writable(&path).unwrap_err();
        assert!(err.to_string().contains("world-writable"));
    }

    fn record_from(p: &Provenance) -> TrustRecord {
        TrustRecord {
            path: p.canonical_path.clone(),
            sha256: p.sha256.clone(),
            device: p.device,
            inode: p.inode,
            mtime: p.mtime,
        }
    }

    /// Inode-only drift (same bytes, different inode) must count as
    /// drift — content equality is not enough to keep trust. The drift
    /// is synthesized on the record rather than via remove+recreate:
    /// filesystems like ext4 routinely reuse a just-freed inode, so a
    /// recreate is NOT guaranteed to change it.
    #[cfg(unix)]
    #[test]
    fn inode_drift_with_identical_content_is_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libra-agent-demo");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let fresh = compute_provenance(&path).unwrap();
        let mut record = record_from(&fresh);
        assert!(!provenance_drifted(&fresh, &record));
        record.inode = record.inode.wrapping_add(1);
        assert_eq!(fresh.sha256, record.sha256, "content markers identical");
        assert!(
            provenance_drifted(&fresh, &record),
            "inode-only change must count as drift"
        );
    }

    /// mtime-only drift (identical bytes and inode, touched timestamp)
    /// must count as drift too — a swapped-back binary shows up as an
    /// mtime change even when hash and inode match again.
    #[cfg(unix)]
    #[test]
    fn mtime_only_drift_is_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libra-agent-demo");
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        let provenance = compute_provenance(&path).unwrap();
        let mut record = record_from(&provenance);
        assert!(
            !provenance_drifted(&provenance, &record),
            "identical markers must not drift"
        );
        record.mtime -= 1; // recorded one second earlier than on-disk state
        assert!(provenance_drifted(&provenance, &record));
        let mut device_record = record_from(&provenance);
        device_record.device = device_record.device.wrapping_add(1);
        assert!(provenance_drifted(&provenance, &device_record));
    }
}
