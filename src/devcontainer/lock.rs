//! `devcontainer-lock.json` — pinning Features to the exact artifact they resolved to.
//!
//! Two problems, one file.
//!
//! **Reproducibility.** A Feature reference like `…/git:1` is a moving tag. Two people building
//! the same config, or the same person a month apart, can get different Features. The lockfile
//! records the digest each id resolved to, and `am` fetches that instead of the tag.
//!
//! **Staleness.** `am` names an image by a hash of its inputs and skips the build when that image
//! exists. Registry and tarball Features cannot be hashed directly without a network round trip
//! per `am start`, which would undo the point of hashing — so the *lockfile* is hashed instead.
//! A moved tag changes the lock, the lock changes the image name, and the next `am start`
//! rebuilds. Nothing is fetched on the fast path.
//!
//! The format is the reference implementation's, so a repo can be shared with tooling that
//! writes the same file — including this one, which keeps its own under `.devcontainer/`. Local
//! Features are deliberately absent: the spec excludes them, and `am` hashes their files
//! directly instead, which is both cheaper and exact.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One Feature's entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    /// The Feature's own `version`, for a human reading the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A qualified id carrying the digest, or — for a tarball — the URL it came from.
    pub resolved: String,
    /// `sha256:` and the hex digest of the artifact: the manifest for a registry Feature, the
    /// downloaded bytes for a tarball.
    pub integrity: String,
    /// The Feature's own `dependsOn` keys, as it wrote them.
    #[serde(default, rename = "dependsOn", skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default)]
    pub features: BTreeMap<String, LockEntry>,
}

impl Lockfile {
    /// The digest a registry Feature id is pinned to, if any.
    ///
    /// Ids are compared lowercased, which the spec requires — a registry reference is
    /// case-insensitive and a lockfile written by another tool will already be folded.
    pub fn digest_for(&self, id: &str) -> Option<&str> {
        let entry = self.features.get(&id.to_lowercase())?;
        entry.resolved.rsplit_once('@').map(|(_, digest)| digest)
    }

    /// The integrity hash recorded for an id, used to verify a tarball download.
    pub fn integrity_for(&self, id: &str) -> Option<&str> {
        Some(self.features.get(&id.to_lowercase())?.integrity.as_str())
    }

    pub fn insert(&mut self, id: &str, entry: LockEntry) {
        self.features.insert(id.to_lowercase(), entry);
    }

    /// Canonical bytes, for hashing and for writing.
    ///
    /// `BTreeMap` keys sort, so the same set of Features always renders identically no matter
    /// what order they resolved in — which is what makes this safe to fold into an image name.
    pub fn canonical(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }
}

/// Where the lockfile lives: beside the `devcontainer.json` it belongs to.
pub fn path(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("devcontainer-lock.json")
}

/// Read the lockfile, or an empty one.
///
/// A malformed lockfile is treated as absent rather than fatal. It is a cache of resolutions,
/// not a source of truth about what the user asked for, and refusing to start a session over a
/// file `am` can rewrite from scratch would be the wrong trade.
pub fn load(config_path: &Path) -> Lockfile {
    std::fs::read_to_string(path(config_path))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the lockfile, unless it already says exactly this.
///
/// The no-op case matters: `am start` runs on every session, and rewriting a checked-in file
/// with identical bytes would show up as a spurious modification in the worktree.
pub fn save(config_path: &Path, lock: &Lockfile) -> Result<()> {
    if lock.features.is_empty() {
        return Ok(());
    }
    let target = path(config_path);
    let rendered = format!("{}\n", lock.canonical());
    if std::fs::read_to_string(&target).is_ok_and(|existing| existing == rendered) {
        return Ok(());
    }
    std::fs::write(&target, rendered)
        .with_context(|| format!("writing {}", target.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(resolved: &str, integrity: &str) -> LockEntry {
        LockEntry {
            version: Some("1.2.3".to_string()),
            resolved: resolved.to_string(),
            integrity: integrity.to_string(),
            depends_on: Vec::new(),
        }
    }

    #[test]
    fn a_pinned_id_resolves_to_its_digest() {
        let mut lock = Lockfile::default();
        lock.insert(
            "ghcr.io/devcontainers/features/git:1",
            entry("ghcr.io/devcontainers/features/git@sha256:abc", "sha256:abc"),
        );
        assert_eq!(lock.digest_for("ghcr.io/devcontainers/features/git:1"), Some("sha256:abc"));
        assert_eq!(lock.digest_for("ghcr.io/devcontainers/features/node:1"), None);
    }

    #[test]
    fn ids_are_compared_without_regard_to_case() {
        // The spec folds keys to lowercase, so a lockfile written by another tool has already
        // done this and a config naming the Feature differently must still match.
        let mut lock = Lockfile::default();
        lock.insert("GHCR.io/Devcontainers/Features/Git:1", entry("x@sha256:abc", "sha256:abc"));
        assert!(lock.features.contains_key("ghcr.io/devcontainers/features/git:1"));
        assert_eq!(lock.digest_for("ghcr.io/devcontainers/features/GIT:1"), Some("sha256:abc"));
    }

    #[test]
    fn a_tarball_entry_has_no_digest_to_pin_but_still_has_integrity() {
        // `resolved` is the URL for a tarball, so there is no `@digest` to fetch by — the
        // integrity hash is what the download is checked against instead.
        let mut lock = Lockfile::default();
        lock.insert("https://example.com/f.tgz", entry("https://example.com/f.tgz", "sha256:def"));
        assert_eq!(lock.digest_for("https://example.com/f.tgz"), None);
        assert_eq!(lock.integrity_for("https://example.com/f.tgz"), Some("sha256:def"));
    }

    #[test]
    fn the_rendering_is_stable_regardless_of_insertion_order() {
        // This is what lets the file be folded into an image name: the same set of Features
        // must render identically however they happened to resolve.
        let mut one = Lockfile::default();
        one.insert("b", entry("b@sha256:2", "sha256:2"));
        one.insert("a", entry("a@sha256:1", "sha256:1"));
        let mut two = Lockfile::default();
        two.insert("a", entry("a@sha256:1", "sha256:1"));
        two.insert("b", entry("b@sha256:2", "sha256:2"));
        assert_eq!(one.canonical(), two.canonical());
    }

    #[test]
    fn a_malformed_lockfile_reads_as_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("devcontainer.json");
        std::fs::write(path(&config), "{ not json").unwrap();
        assert_eq!(load(&config), Lockfile::default());
    }

    #[test]
    fn saving_the_same_content_twice_does_not_touch_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("devcontainer.json");
        let mut lock = Lockfile::default();
        lock.insert("a", entry("a@sha256:1", "sha256:1"));

        save(&config, &lock).unwrap();
        let first = std::fs::metadata(path(&config)).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        save(&config, &lock).unwrap();
        let second = std::fs::metadata(path(&config)).unwrap().modified().unwrap();

        assert_eq!(first, second, "an unchanged lockfile must not be rewritten");
    }

    #[test]
    fn the_shape_matches_the_reference_implementation() {
        // Byte-compatible with what `devcontainer build` writes, so a repo can be shared with
        // tooling that maintains the same file.
        let mut lock = Lockfile::default();
        lock.insert(
            "ghcr.io/devcontainers/features/git:1",
            LockEntry {
                version: Some("1.3.8".to_string()),
                resolved: "ghcr.io/devcontainers/features/git@sha256:fd75".to_string(),
                integrity: "sha256:fd75".to_string(),
                depends_on: Vec::new(),
            },
        );
        let parsed: serde_json::Value = serde_json::from_str(&lock.canonical()).unwrap();
        let entry = &parsed["features"]["ghcr.io/devcontainers/features/git:1"];
        assert_eq!(entry["version"], "1.3.8");
        assert_eq!(entry["resolved"], "ghcr.io/devcontainers/features/git@sha256:fd75");
        assert_eq!(entry["integrity"], "sha256:fd75");
        // Absent rather than null when there is nothing to say.
        assert!(entry.get("dependsOn").is_none());
    }

    #[test]
    fn a_lockfile_written_by_the_reference_implementation_round_trips() {
        let text = r#"{
  "features": {
    "ghcr.io/devcontainers/features/git:1": {
      "version": "1.3.8",
      "resolved": "ghcr.io/devcontainers/features/git@sha256:fd75",
      "integrity": "sha256:fd75"
    }
  }
}"#;
        let lock: Lockfile = serde_json::from_str(text).unwrap();
        assert_eq!(lock.digest_for("ghcr.io/devcontainers/features/git:1"), Some("sha256:fd75"));
    }
}

#[cfg(test)]
mod repo_roundtrip {
    /// The repo keeps a lockfile the reference implementation wrote. `am` now writes this file
    /// too, so its rendering has to match byte for byte — otherwise every `am start` in this
    /// repo would show a spurious modification.
    #[test]
    fn ams_rendering_matches_the_committed_lockfile() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.devcontainer/devcontainer-lock.json");
        let original = std::fs::read_to_string(path).expect("repo lockfile");
        let lock: super::Lockfile = serde_json::from_str(&original).expect("parses");
        assert_eq!(format!("{}\n", lock.canonical()), original);
    }
}
