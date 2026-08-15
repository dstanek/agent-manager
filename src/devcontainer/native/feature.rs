//! Feature metadata, option resolution, and install ordering.
//!
//! A Feature's `devcontainer-feature.json` plays two roles, and keeping them apart is what
//! makes this module small:
//!
//! - **Build inputs** — `options` (defaults + types) and `installsAfter` (ordering). Modelled
//!   as typed fields, because `am` computes with them.
//! - **Runtime contributions** — `entrypoint`, `mounts`, `containerEnv`, `customizations`, …
//!   These are *not* modelled. They are filtered to the image-metadata schema and copied into
//!   the label verbatim, so a property `am` has never heard of still reaches the run path.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use super::oci::FeatureRef;
use crate::error::AmError;

/// The properties that survive from a Feature or a `devcontainer.json` into the
/// `devcontainer.metadata` image label.
///
/// Taken from the devcontainer image-metadata schema. Anything absent here is a build-time or
/// editor-only concern (`options`, `installsAfter`, `image`, `features`, `name`, …) and is
/// dropped — which is exactly what the reference CLI does, and what the captured fixture
/// `cli-git-label.json` pins.
const METADATA_KEYS: &[&str] = &[
    "init",
    "privileged",
    "capAdd",
    "securityOpt",
    "entrypoint",
    "mounts",
    "containerEnv",
    "remoteEnv",
    "containerUser",
    "remoteUser",
    "updateRemoteUserUID",
    "userEnvProbe",
    "overrideCommand",
    "waitFor",
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
    "customizations",
    "hostRequirements",
];

/// A Feature's declared option.
#[derive(Debug, Clone, Deserialize)]
pub struct OptionDef {
    #[serde(default)]
    pub default: Option<Value>,
}

/// The parts of `devcontainer-feature.json` that `am` computes with.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureMetadata {
    /// The Feature's own short id (`git`). Not used for identity — the label and the build
    /// directory both key off the user-written reference — but kept because it is what a
    /// Feature author sees when debugging a mismatch.
    #[allow(dead_code)]
    pub id: Option<String>,
    pub version: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub documentation_url: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, OptionDef>,
    /// Soft dependencies: ordering only, and only against Features already being installed.
    /// Never pulls anything in, and carries no options — unlike [`Self::depends_on`].
    #[serde(default)]
    pub installs_after: Vec<String>,
    /// Hard dependencies, same shape as the `features` object in a `devcontainer.json`: a map
    /// of Feature id to its options. Resolved recursively, so a dependency can pull in Features
    /// the config never named.
    #[serde(default)]
    pub depends_on: Option<Value>,
}

/// A Feature resolved far enough to be installed.
#[derive(Debug, Clone)]
pub struct ResolvedFeature {
    pub reference: FeatureRef,
    pub metadata: FeatureMetadata,
    /// The raw `devcontainer-feature.json`, kept for the label filter.
    pub raw: Value,
    /// Option values as install-time env vars, defaults already applied.
    pub options: BTreeMap<String, String>,
    /// The options as the *caller* wrote them, before defaults were merged in. Only the
    /// round-stable sort uses these: the spec tie-breaks on user-defined options, so a default
    /// the author never typed must not affect where the Feature lands.
    pub supplied: BTreeMap<String, Value>,
    /// Content digest of the Feature's layer. Two Features are the same Feature when their
    /// contents and options match, and the digest is what "same contents" means for a registry
    /// Feature — a moving tag like `:1` resolving to the same bytes is not a second install.
    pub digest: String,
    /// [`Self::key`] of every Feature this one hard-depends on, resolved. Keys rather than
    /// indices because ordering permutes the set.
    pub hard_deps: Vec<String>,
}

impl ResolvedFeature {
    /// Identity for deduplication and for hard-dependency links.
    ///
    /// Contents plus options, per the spec's Feature-equality rule. Deliberately *not* the
    /// written id: two ids that resolve to the same digest with the same options are one node,
    /// and the same id with different options is two.
    pub fn key(&self) -> String {
        format!("{}|{}", self.digest, canonical_options(&self.supplied))
    }
}

/// Identity for a Feature that has been *asked for* but not yet fetched.
///
/// The written id rather than a digest, because that is all a caller has before the manifest
/// comes back. Used to collapse repeat requests for the same thing — including the cycle a
/// `dependsOn` graph can contain — before spending a round trip on them.
pub fn request_key(id: &str, options: &BTreeMap<String, Value>) -> String {
    format!("{id}|{}", canonical_options(options))
}

/// Options rendered to a stable string, for identity and for the sort's key/value tie-breaks.
fn canonical_options(options: &BTreeMap<String, Value>) -> String {
    options
        .iter()
        .map(|(k, v)| format!("{k}={}", stringify(v)))
        .collect::<Vec<_>>()
        .join(",")
}

impl ResolvedFeature {
    /// The directory name this Feature gets in the build context (`git` → `git_0`).
    pub fn dir_name(&self, index: usize) -> String {
        // Non-alphanumerics would break the shell contract in the generated install wrapper.
        let sanitized: String = self
            .reference
            .name()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        format!("{sanitized}_{index}")
    }

    /// This Feature's contribution to the image metadata label.
    ///
    /// The `id` is the user-written reference rather than the Feature's own short `id`, because
    /// that is what identifies the contribution when a label is read back later.
    pub fn label_snippet(&self) -> Value {
        let mut out = serde_json::Map::new();
        // `id` leads, then the Feature's own properties in the order it declared them —
        // which is what the reference CLI emits, and what makes the label byte-comparable.
        out.insert("id".to_string(), Value::String(self.reference.raw.clone()));
        out.extend(metadata_properties(&self.raw));
        Value::Object(out)
    }
}

/// Parse a `devcontainer-feature.json`.
pub fn parse_metadata(text: &str) -> Result<(FeatureMetadata, Value)> {
    let raw: Value = serde_json_lenient::from_str(text)
        .with_context(|| "parsing devcontainer-feature.json")?;
    let metadata: FeatureMetadata = serde_json_lenient::from_str(text)
        .with_context(|| "parsing devcontainer-feature.json")?;
    Ok((metadata, raw))
}

/// Reduce a `devcontainer.json` to its image-metadata contribution.
///
/// This is the final element of the label, so it carries the highest precedence in the merge.
pub fn config_label_snippet(raw: &Value) -> Value {
    Value::Object(metadata_properties(raw).collect())
}

/// The image-metadata properties of an object, in **schema** order.
///
/// Walking [`METADATA_KEYS`] rather than the source object's own key order is what matches the
/// reference CLI: it emits properties in the order its metadata type declares them, not the
/// order the author wrote them. A `devcontainer.json` saying `remoteUser` before `containerEnv`
/// still produces `containerEnv` first — which the differential test pins.
///
/// This ordering only exists to keep the label byte-comparable. Nothing downstream depends on
/// it: [`crate::devcontainer::merge`] reads properties by name.
fn metadata_properties(raw: &Value) -> impl Iterator<Item = (String, Value)> + '_ {
    METADATA_KEYS.iter().filter_map(move |key| {
        raw.get(*key).map(|value| ((*key).to_string(), value.clone()))
    })
}

/// Merge user-supplied option values over the Feature's declared defaults.
///
/// Values become environment variables for `install.sh`, so everything is stringified: the
/// contract is `VERSION="os-provided"`, never a JSON literal.
pub fn resolve_options(
    metadata: &FeatureMetadata,
    supplied: &BTreeMap<String, Value>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (name, def) in &metadata.options {
        if let Some(default) = &def.default {
            out.insert(name.to_uppercase(), stringify(default));
        }
    }
    for (name, value) in supplied {
        // An option the Feature never declared is still passed through: some Features read
        // env vars they do not document, and dropping the value would silently ignore it.
        out.insert(name.to_uppercase(), stringify(value));
    }
    out
}

/// Render an option value the way the install contract expects.
fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Order Features for installation, by the spec's round-based algorithm.
///
/// Each round takes **every** Feature whose dependencies are already placed, sorts that whole
/// group by [`round_stable_sort`], and commits it. The round is the part that is easy to get
/// wrong: picking one Feature at a time and re-testing — the obvious reading, and what this
/// function used to do — interleaves independent chains differently. Given `a` after `b` and
/// `c` after `d`, one-at-a-time yields `b, a, d, c` while the spec yields `b, d, a, c`, because
/// `b` and `d` are both eligible in round one and sort together. Any config with two unrelated
/// `installsAfter` chains diverges, which is most real configs.
///
/// Hard dependencies (`dependsOn`) are always part of the set, since the resolver pulled them
/// in. Soft ones (`installsAfter`) constrain only Features that happen to be present.
///
/// `overrideFeatureInstallOrder` is the spec's `roundPriority`, which would slot in here as a
/// filter on each round; the caller still rejects configs using it.
pub fn install_order(features: Vec<ResolvedFeature>) -> Result<Vec<ResolvedFeature>> {
    let present: Vec<String> = features.iter().map(|f| f.reference.untagged()).collect();
    let mut worklist = features;
    let mut ordered: Vec<ResolvedFeature> = Vec::with_capacity(worklist.len());
    let mut placed_keys: Vec<String> = Vec::new();
    let mut placed_ids: Vec<String> = Vec::new();

    while !worklist.is_empty() {
        // Eligibility is judged against what was placed in *previous* rounds, so two Features
        // that could order either way both land in this round rather than one waiting on the
        // other.
        let (mut round, rest): (Vec<_>, Vec<_>) = worklist.into_iter().partition(|f| {
            let hard = f.hard_deps.iter().all(|k| placed_keys.iter().any(|p| p == k));
            let soft = f.metadata.installs_after.iter().all(|dep| {
                let dep = dep.split(':').next().unwrap_or(dep);
                // A soft dependency on a Feature that is not part of this build is satisfied
                // by definition — installsAfter is an ordering hint, not a requirement.
                !present.iter().any(|p| p == dep) || placed_ids.iter().any(|p| p == dep)
            });
            hard && soft
        });

        if round.is_empty() {
            let cycle = rest
                .iter()
                .map(|f| f.reference.raw.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(AmError::DevcontainerBuildFailed(format!(
                "these Features depend on each other circularly: {cycle}"
            ))
            .into());
        }

        round.sort_by(round_stable_sort);
        placed_keys.extend(round.iter().map(ResolvedFeature::key));
        placed_ids.extend(round.iter().map(|f| f.reference.untagged()));
        ordered.extend(round);
        worklist = rest;
    }
    Ok(ordered)
}

/// The spec's "Round Stable Sort": the tie-break among Features committed in the same round.
///
/// The order of the comparisons is the spec's, and the third one is inverted on purpose —
/// *more* user-defined options sorts first.
fn round_stable_sort(a: &ResolvedFeature, b: &ResolvedFeature) -> std::cmp::Ordering {
    a.reference
        .untagged()
        .cmp(&b.reference.untagged())
        .then_with(|| compare_versions(&a.reference.tag, &b.reference.tag))
        .then_with(|| b.supplied.len().cmp(&a.supplied.len()))
        .then_with(|| a.supplied.keys().cmp(b.supplied.keys()))
        .then_with(|| {
            a.supplied.values().map(stringify).cmp(b.supplied.values().map(stringify))
        })
        .then_with(|| a.digest.cmp(&b.digest))
}

/// Compare version tags oldest-to-newest.
///
/// Dot-separated, numeric where both sides are numeric so `2` sorts after `10`'s prefix rather
/// than before it, and lexicographic otherwise. Tags that are not versions at all (`latest`)
/// compare as strings, which is arbitrary but stable — and the digest tie-break below it means
/// two Features never compare equal by accident.
fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(x), Some(y)) => {
                let ordering = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(xn), Ok(yn)) => xn.cmp(&yn),
                    _ => x.cmp(y),
                };
                if ordering != std::cmp::Ordering::Equal {
                    return ordering;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::native::oci;

    fn feature(id: &str, installs_after: &[&str]) -> ResolvedFeature {
        let oci::FeatureSource::Registry(reference) = oci::parse_ref(id) else {
            panic!("test ids must be registry refs");
        };
        // A distinct digest per id keeps the sort's last tie-break from ever deciding, so a
        // test that means to pin an earlier comparison cannot pass by accident.
        let digest = format!("sha256:{id}");
        ResolvedFeature {
            reference,
            metadata: FeatureMetadata {
                installs_after: installs_after.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            raw: Value::Object(Default::default()),
            options: BTreeMap::new(),
            supplied: BTreeMap::new(),
            digest,
            hard_deps: Vec::new(),
        }
    }

    /// A Feature that hard-depends on others, linked by the same key scheme the resolver uses.
    fn feature_depending_on(id: &str, hard: &[&str]) -> ResolvedFeature {
        let mut f = feature(id, &[]);
        f.hard_deps = hard
            .iter()
            .map(|dep| feature(dep, &[]).key())
            .collect();
        f
    }

    fn ids(features: &[ResolvedFeature]) -> Vec<String> {
        features.iter().map(|f| f.reference.raw.clone()).collect()
    }

    #[test]
    fn parses_the_captured_feature_json() {
        let text =
            include_str!("../../../tests/fixtures/devcontainer/native/git-devcontainer-feature.json");
        let (metadata, _raw) = parse_metadata(text).unwrap();
        assert_eq!(metadata.id.as_deref(), Some("git"));
        assert_eq!(
            metadata.installs_after,
            vec!["ghcr.io/devcontainers/features/common-utils"]
        );
        assert!(metadata.options.contains_key("version"));
        assert!(metadata.depends_on.is_none());
    }

    #[test]
    fn option_defaults_match_the_cli_env_file() {
        let text =
            include_str!("../../../tests/fixtures/devcontainer/native/git-devcontainer-feature.json");
        let (metadata, _) = parse_metadata(text).unwrap();
        let options = resolve_options(&metadata, &BTreeMap::new());
        // The CLI wrote exactly VERSION="os-provided" / PPA="true" for this Feature.
        assert_eq!(options.get("VERSION").map(String::as_str), Some("os-provided"));
        assert_eq!(options.get("PPA").map(String::as_str), Some("true"));
    }

    #[test]
    fn supplied_options_override_defaults() {
        let text =
            include_str!("../../../tests/fixtures/devcontainer/native/git-devcontainer-feature.json");
        let (metadata, _) = parse_metadata(text).unwrap();
        let supplied = BTreeMap::from([("version".to_string(), Value::String("latest".into()))]);
        let options = resolve_options(&metadata, &supplied);
        assert_eq!(options.get("VERSION").map(String::as_str), Some("latest"));
        assert_eq!(options.get("PPA").map(String::as_str), Some("true"));
    }

    #[test]
    fn booleans_and_numbers_are_stringified_not_json_encoded() {
        let metadata = FeatureMetadata::default();
        let supplied = BTreeMap::from([
            ("flag".to_string(), Value::Bool(false)),
            ("count".to_string(), Value::Number(3.into())),
        ]);
        let options = resolve_options(&metadata, &supplied);
        assert_eq!(options.get("FLAG").map(String::as_str), Some("false"));
        assert_eq!(options.get("COUNT").map(String::as_str), Some("3"));
    }

    #[test]
    fn label_snippet_keeps_only_metadata_properties() {
        let text =
            include_str!("../../../tests/fixtures/devcontainer/native/git-devcontainer-feature.json");
        let (metadata, raw) = parse_metadata(text).unwrap();
        let oci::FeatureSource::Registry(reference) =
            oci::parse_ref("ghcr.io/devcontainers/features/git:1")
        else {
            unreachable!()
        };
        let resolved = ResolvedFeature {
            reference,
            metadata,
            raw,
            options: BTreeMap::new(),
            supplied: BTreeMap::new(),
            digest: "sha256:test".to_string(),
            hard_deps: Vec::new(),
        };
        let snippet = resolved.label_snippet();
        let obj = snippet.as_object().unwrap();

        // What the CLI kept.
        assert_eq!(
            obj.get("id").and_then(Value::as_str),
            Some("ghcr.io/devcontainers/features/git:1")
        );
        assert!(obj.contains_key("customizations"));
        // What the CLI dropped — build-time only.
        for dropped in ["options", "installsAfter", "version", "name", "description"] {
            assert!(!obj.contains_key(dropped), "{dropped} should not reach the label");
        }
    }

    #[test]
    fn config_snippet_drops_build_inputs() {
        let raw: Value = serde_json_lenient::from_str(
            r#"{"image":"debian","features":{"x":{}},"remoteUser":"root","name":"demo"}"#,
        )
        .unwrap();
        let snippet = config_label_snippet(&raw);
        assert_eq!(
            snippet,
            serde_json::json!({"remoteUser": "root"}),
            "only remoteUser is an image-metadata property"
        );
    }

    #[test]
    fn install_order_is_alphabetical_without_constraints() {
        let ordered = install_order(vec![
            feature("ghcr.io/x/features/zebra:1", &[]),
            feature("ghcr.io/x/features/apple:1", &[]),
        ])
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec!["ghcr.io/x/features/apple:1", "ghcr.io/x/features/zebra:1"]
        );
    }

    #[test]
    fn installs_after_overrides_alphabetical_order() {
        // `apple` sorts first but must wait for `zebra`.
        let ordered = install_order(vec![
            feature("ghcr.io/x/features/apple:1", &["ghcr.io/x/features/zebra"]),
            feature("ghcr.io/x/features/zebra:1", &[]),
        ])
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec!["ghcr.io/x/features/zebra:1", "ghcr.io/x/features/apple:1"]
        );
    }

    #[test]
    fn installs_after_an_absent_feature_is_ignored() {
        // git dependsOn common-utils, which this build does not include. Ordering must not
        // deadlock waiting for something that will never be placed.
        let ordered = install_order(vec![feature(
            "ghcr.io/devcontainers/features/git:1",
            &["ghcr.io/devcontainers/features/common-utils"],
        )])
        .unwrap();
        assert_eq!(ids(&ordered), vec!["ghcr.io/devcontainers/features/git:1"]);
    }

    #[test]
    fn a_cycle_is_reported_rather_than_looping_forever() {
        let err = install_order(vec![
            feature("ghcr.io/x/features/a:1", &["ghcr.io/x/features/b"]),
            feature("ghcr.io/x/features/b:1", &["ghcr.io/x/features/a"]),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("circular"), "got: {err}");
    }

    #[test]
    fn independent_chains_interleave_by_round_not_one_at_a_time() {
        // The case that separates the round-based algorithm from the obvious one. Ordering is
        // pinned to what `devcontainer features resolve-dependencies` returns for exactly this
        // config, so it is the reference CLI's answer, not a reading of the spec:
        //
        //   gh-release, common-utils, act, git
        //
        // Taking one eligible Feature at a time instead yields `gh-release, act, common-utils,
        // git` — `act` jumps the queue because it becomes eligible the moment `gh-release`
        // lands, rather than waiting for the round to close.
        let ordered = install_order(vec![
            feature(
                "ghcr.io/devcontainers-extra/features/act:1",
                &["ghcr.io/devcontainers-extra/features/gh-release"],
            ),
            feature("ghcr.io/devcontainers-extra/features/gh-release:1", &[]),
            feature(
                "ghcr.io/devcontainers/features/git:1",
                &["ghcr.io/devcontainers/features/common-utils"],
            ),
            feature("ghcr.io/devcontainers/features/common-utils:2", &[]),
        ])
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec![
                "ghcr.io/devcontainers-extra/features/gh-release:1",
                "ghcr.io/devcontainers/features/common-utils:2",
                "ghcr.io/devcontainers-extra/features/act:1",
                "ghcr.io/devcontainers/features/git:1",
            ]
        );
    }

    #[test]
    fn a_hard_dependency_installs_before_its_dependent() {
        // `apple` sorts first alphabetically and has no installsAfter, but dependsOn is a hard
        // edge and outranks the tie-break.
        let ordered = install_order(vec![
            feature_depending_on("ghcr.io/x/features/apple:1", &["ghcr.io/x/features/zebra:1"]),
            feature("ghcr.io/x/features/zebra:1", &[]),
        ])
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec!["ghcr.io/x/features/zebra:1", "ghcr.io/x/features/apple:1"]
        );
    }

    #[test]
    fn a_hard_dependency_cycle_is_reported() {
        let err = install_order(vec![
            feature_depending_on("ghcr.io/x/features/a:1", &["ghcr.io/x/features/b:1"]),
            feature_depending_on("ghcr.io/x/features/b:1", &["ghcr.io/x/features/a:1"]),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("circularly"), "got: {err}");
        // The message has to name both, or there is nothing to go edit.
        assert!(err.to_string().contains("a:1") && err.to_string().contains("b:1"), "got: {err}");
    }

    #[test]
    fn the_same_feature_at_two_versions_sorts_oldest_first() {
        // Same resource name, so the tie-break falls through to the version tag.
        let ordered = install_order(vec![
            feature("ghcr.io/x/features/node:10", &[]),
            feature("ghcr.io/x/features/node:2", &[]),
        ])
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec!["ghcr.io/x/features/node:2", "ghcr.io/x/features/node:10"],
            "version tags compare numerically, not as strings"
        );
    }

    #[test]
    fn more_user_defined_options_sorts_first() {
        // The one tie-break the spec inverts: greatest number of options wins.
        let mut bare = feature("ghcr.io/x/features/node:1", &[]);
        bare.digest = "sha256:bare".to_string();
        let mut configured = feature("ghcr.io/x/features/node:1", &[]);
        configured.digest = "sha256:configured".to_string();
        configured.supplied =
            BTreeMap::from([("version".to_string(), Value::String("20".to_string()))]);

        let ordered = install_order(vec![bare, configured]).unwrap();
        assert_eq!(
            ordered.iter().map(|f| f.digest.as_str()).collect::<Vec<_>>(),
            vec!["sha256:configured", "sha256:bare"]
        );
    }

    #[test]
    fn version_comparison_is_numeric_per_segment() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("1", "2"), Ordering::Less);
        assert_eq!(compare_versions("2", "10"), Ordering::Less);
        assert_eq!(compare_versions("1.2.3", "1.10.0"), Ordering::Less);
        assert_eq!(compare_versions("1.2", "1.2"), Ordering::Equal);
        // A shorter tag is a prefix of the longer one and sorts first.
        assert_eq!(compare_versions("1", "1.0"), Ordering::Less);
        // Not a version at all: stable, and the digest tie-break decides after it.
        assert_eq!(compare_versions("latest", "latest"), Ordering::Equal);
    }

    #[test]
    fn identity_is_contents_plus_options_not_the_written_id() {
        // A moving tag and the version it currently points at are one Feature...
        let mut moving = feature("ghcr.io/x/features/node:1", &[]);
        moving.digest = "sha256:same".to_string();
        let mut pinned = feature("ghcr.io/x/features/node:1.2.3", &[]);
        pinned.digest = "sha256:same".to_string();
        assert_eq!(moving.key(), pinned.key());

        // ...but the same contents with different options are two.
        let mut configured = pinned.clone();
        configured.supplied =
            BTreeMap::from([("version".to_string(), Value::String("20".to_string()))]);
        assert_ne!(pinned.key(), configured.key());
    }

    #[test]
    fn install_order_is_stable_regardless_of_input_order() {
        let forward = install_order(vec![
            feature("ghcr.io/x/features/a:1", &[]),
            feature("ghcr.io/x/features/b:1", &[]),
            feature("ghcr.io/x/features/c:1", &[]),
        ])
        .unwrap();
        let reversed = install_order(vec![
            feature("ghcr.io/x/features/c:1", &[]),
            feature("ghcr.io/x/features/b:1", &[]),
            feature("ghcr.io/x/features/a:1", &[]),
        ])
        .unwrap();
        assert_eq!(ids(&forward), ids(&reversed));
    }
}
