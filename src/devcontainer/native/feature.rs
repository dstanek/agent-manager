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
    #[serde(default)]
    pub installs_after: Vec<String>,
    /// Presence alone matters: `dependsOn` needs round-trip resolution (a dependency can pull
    /// in Features the config never named), which this pass does not implement.
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

/// Order Features for installation.
///
/// The rule implemented here is the spec's simple case: start alphabetically by id, then
/// repeatedly take the first Feature whose `installsAfter` dependencies — counting only
/// Features actually being installed — are already placed.
///
/// `dependsOn` and `overrideFeatureInstallOrder` are **not** handled; the caller rejects
/// configs using them before reaching this point.
pub fn install_order(features: Vec<ResolvedFeature>) -> Result<Vec<ResolvedFeature>> {
    let mut remaining = features;
    // Alphabetical by id is the documented tie-break, and it is also what makes the generated
    // Dockerfile stable across runs — which is what lets the image hash stay meaningful.
    remaining.sort_by(|a, b| a.reference.raw.cmp(&b.reference.raw));

    let present: Vec<String> = remaining.iter().map(|f| f.reference.untagged()).collect();
    let mut ordered: Vec<ResolvedFeature> = Vec::with_capacity(remaining.len());
    let mut placed: Vec<String> = Vec::new();

    while !remaining.is_empty() {
        let next = remaining.iter().position(|f| {
            f.metadata.installs_after.iter().all(|dep| {
                let dep = dep.split(':').next().unwrap_or(dep);
                // A dependency on a Feature that is not part of this build is satisfied by
                // definition — installsAfter is an ordering hint, not a requirement.
                !present.iter().any(|p| p == dep) || placed.iter().any(|p| p == dep)
            })
        });

        match next {
            Some(index) => {
                let feature = remaining.remove(index);
                placed.push(feature.reference.untagged());
                ordered.push(feature);
            }
            None => {
                let cycle = remaining
                    .iter()
                    .map(|f| f.reference.raw.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(AmError::DevcontainerBuildFailed(format!(
                    "these Features have a circular installsAfter relationship: {cycle}"
                ))
                .into());
            }
        }
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::native::oci;

    fn feature(id: &str, installs_after: &[&str]) -> ResolvedFeature {
        let oci::FeatureSource::Registry(reference) = oci::parse_ref(id) else {
            panic!("test ids must be registry refs");
        };
        ResolvedFeature {
            reference,
            metadata: FeatureMetadata {
                installs_after: installs_after.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            raw: Value::Object(Default::default()),
            options: BTreeMap::new(),
        }
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
