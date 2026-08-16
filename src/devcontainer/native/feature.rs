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

/// The properties that survive into the `devcontainer.metadata` image label, per source.
///
/// Anything absent from both lists is a build-time or editor-only concern (`options`,
/// `installsAfter`, `image`, `features`, `name`, …) and is dropped — which is what the reference
/// CLI does, and what the captured fixtures pin.
///
/// What a **Feature** contributes, in the order the reference CLI emits it.
///
/// Transcribed from the CLI's own `pickFeatureProperties` rather than inferred from a probe: a
/// probe only shows the properties the Feature it was built from happened to declare, which is
/// how the five lifecycle hooks were missed here originally — a Feature's `postCreateCommand`
/// was dropped, so `git-lfs` never pulled its artifacts.
///
/// Still narrower than a config's. A Feature declaring `forwardPorts` or `hostRequirements` has
/// those dropped, and `containerEnv` is dropped *from the label* — but only because the builder
/// bakes it into the image as `ENV` instead. See `dockerfile::render`.
const FEATURE_METADATA_KEYS: &[&str] = &[
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
    "init",
    "privileged",
    "capAdd",
    "securityOpt",
    "entrypoint",
    "mounts",
    "customizations",
];

/// What a **`devcontainer.json`** contributes, in the order the reference CLI emits it.
///
/// A different list *and a different order* from the Feature one — the two were conflated here
/// originally, which is a divergence any config setting both `customizations` and one of
/// `init`/`mounts`/`containerEnv` would have hit: the CLI emits `customizations` seventh, while
/// the Feature order puts it last. `entrypoint` is absent because `devcontainer.json` has no
/// such property; only Features contribute entrypoints.
///
/// Transcribed from the CLI's `pickConfigProperties`. A probe cannot produce this list on its
/// own — it only reveals what the probe config declared, which is how `shutdownAction` was
/// missing until a review caught it.
const CONFIG_METADATA_KEYS: &[&str] = &[
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
    "waitFor",
    "customizations",
    "mounts",
    "containerEnv",
    "containerUser",
    "init",
    "privileged",
    "capAdd",
    "securityOpt",
    "remoteUser",
    "userEnvProbe",
    "remoteEnv",
    "overrideCommand",
    "portsAttributes",
    "otherPortsAttributes",
    "forwardPorts",
    "shutdownAction",
    "updateRemoteUserUID",
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
    /// The Feature's own short id (`git`). Not identity — the label keys off the user-written
    /// reference — but it does name the Feature's directory in the build context, which is
    /// what the reference CLI does and which only diverges for a local Feature whose folder
    /// name differs from the id it declares.
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
    /// Content digest. Two Features are the same Feature when their contents and options
    /// match, and this is what "same contents" means per source: the **manifest** digest for a
    /// registry Feature — not its layer's, since two manifests can share a layer while
    /// differing in metadata or `dependsOn` — the tarball's bytes for a tarball, and the
    /// resolved path for a local Feature, which the spec says is always distinct.
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
    ///
    /// Keyed on the Feature's *declared* id, which is what the reference CLI uses: a local
    /// Feature in a folder called `folderx` declaring `"id": "featy"` is staged as `featy_0`.
    /// For a registry Feature the two normally coincide. Falls back to the reference's last
    /// path segment for a Feature that declares no id at all.
    pub fn dir_name(&self, index: usize) -> String {
        let name = self.metadata.id.as_deref().unwrap_or_else(|| self.reference.name());
        // Non-alphanumerics would break the shell contract in the generated install wrapper.
        let sanitized: String =
            name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
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
        out.extend(metadata_properties(&self.raw, FEATURE_METADATA_KEYS));
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
    Value::Object(metadata_properties(raw, CONFIG_METADATA_KEYS).collect())
}

/// The image-metadata properties of an object, in **schema** order.
///
/// Walking a fixed key list rather than the source object's own key order is what matches the
/// reference CLI: it emits properties in the order its metadata type declares them, not the
/// order the author wrote them. A `devcontainer.json` saying `remoteUser` before `containerEnv`
/// still produces `containerEnv` first — which the differential test pins.
///
/// Which list depends on where the object came from; see [`FEATURE_METADATA_KEYS`] and
/// [`CONFIG_METADATA_KEYS`], which differ in both membership and order.
///
/// This ordering only exists to keep the label byte-comparable. Nothing downstream depends on
/// it: [`crate::devcontainer::merge`] reads properties by name.
fn metadata_properties<'a>(
    raw: &'a Value,
    keys: &'a [&'a str],
) -> impl Iterator<Item = (String, Value)> + 'a {
    keys.iter().filter_map(move |key| {
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
            out.insert(option_env_name(name), stringify(default));
        }
    }
    for (name, value) in supplied {
        // An option the Feature never declared is still passed through: some Features read
        // env vars they do not document, and dropping the value would silently ignore it.
        out.insert(option_env_name(name), stringify(value));
    }
    out
}

/// Turn an option name into the environment variable the install contract exposes it as.
///
/// Uppercasing alone is not enough: an option called `my-option` would become `MY-OPTION`, which
/// is not a valid shell assignment, so the generated env file would fail to source and take the
/// whole Feature install down with it.
///
/// The rule is the reference CLI's, established by building a Feature whose options exercise
/// each case rather than by reading the prose:
///
/// | option | variable |
/// |---|---|
/// | `my-option` | `MY_OPTION` |
/// | `dotted.name` | `DOTTED_NAME` |
/// | `a--b` | `A__B` — repeated separators are *not* collapsed |
/// | `2fa` | `_FA` — a leading digit is replaced, not prefixed |
/// | `12ab` | `_AB` — and a leading *run* collapses to one underscore |
/// | `__dunder` | `_DUNDER` |
fn option_env_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let rest = replaced.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    let leading = if rest.len() == replaced.len() { "" } else { "_" };
    format!("{leading}{rest}").to_uppercase()
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
/// `override_order` is `overrideFeatureInstallOrder` from the `devcontainer.json`. It does not
/// place Features directly: it raises their `roundPriority`, and a round commits only its
/// highest-priority members, returning the rest to the worklist. So an override can reorder
/// Features that were free to go in either order, but it cannot make one jump a dependency —
/// eligibility is decided before priority is looked at.
pub fn install_order(
    features: Vec<ResolvedFeature>,
    override_order: &[String],
) -> Result<Vec<ResolvedFeature>> {
    let present: Vec<String> = features.iter().map(|f| f.reference.untagged()).collect();
    let mut worklist = features;
    let mut ordered: Vec<ResolvedFeature> = Vec::with_capacity(worklist.len());
    let mut placed_keys: Vec<String> = Vec::new();
    let mut placed_ids: Vec<String> = Vec::new();

    while !worklist.is_empty() {
        // Eligibility is judged against what was placed in *previous* rounds, so two Features
        // that could order either way both land in this round rather than one waiting on the
        // other.
        let (eligible, mut rest): (Vec<_>, Vec<_>) = worklist.into_iter().partition(|f| {
            let hard = f.hard_deps.iter().all(|k| placed_keys.iter().any(|p| p == k));
            let soft = f.metadata.installs_after.iter().all(|dep| {
                let dep = dep.split(':').next().unwrap_or(dep);
                // A soft dependency on a Feature that is not part of this build is satisfied
                // by definition — installsAfter is an ordering hint, not a requirement.
                !present.iter().any(|p| p == dep) || placed_ids.iter().any(|p| p == dep)
            });
            hard && soft
        });

        if eligible.is_empty() {
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

        // Only the highest-priority eligible Features commit; the others go back and compete
        // again next round. `max` is always attained, so a round is never empty here.
        let top = eligible
            .iter()
            .map(|f| round_priority(f, override_order))
            .max()
            .unwrap_or(0);
        let (mut round, deferred): (Vec<_>, Vec<_>) = eligible
            .into_iter()
            .partition(|f| round_priority(f, override_order) == top);
        rest.extend(deferred);

        round.sort_by(round_stable_sort);
        placed_keys.extend(round.iter().map(ResolvedFeature::key));
        placed_ids.extend(round.iter().map(|f| f.reference.untagged()));
        ordered.extend(round);
        worklist = rest;
    }
    Ok(ordered)
}

/// A Feature's `roundPriority`: `n - idx` for the `overrideFeatureInstallOrder` entry naming it,
/// and 0 for everything the list does not mention.
///
/// An entry matches either the fully qualified name without its tag, or the Feature's short
/// alias (`git` for `ghcr.io/devcontainers/features/git`) — the reference CLI accepts both, and
/// the short form is what people actually write. An entry matching nothing being installed is
/// ignored; the CLI instead resolves every entry and errors if it cannot, but since such an
/// entry changes no ordering, the resulting label — the thing that is actually the contract —
/// is identical either way.
fn round_priority(feature: &ResolvedFeature, override_order: &[String]) -> i64 {
    let untagged = feature.reference.untagged();
    let alias = feature.reference.name();
    override_order
        .iter()
        .position(|entry| {
            let entry = entry.rsplit_once(':').map_or(entry.as_str(), |(head, tail)| {
                // Only a trailing tag, not the port in `localhost:5000/…`.
                if tail.contains('/') { entry } else { head }
            });
            entry == untagged || entry == alias
        })
        .map(|idx| (override_order.len() - idx) as i64)
        .unwrap_or(0)
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
        let reference = oci::parse_ref(id);
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

    /// Ordering without an `overrideFeatureInstallOrder`, which is the overwhelmingly common
    /// case. Shadows the real one so the tests about priority have to name it explicitly.
    fn install_order(features: Vec<ResolvedFeature>) -> Result<Vec<ResolvedFeature>> {
        super::install_order(features, &[])
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
    fn option_names_become_valid_shell_variables() {
        // Every case measured against a real `devcontainer build` — see `option_env_name`.
        // `MY-OPTION` would not be a valid assignment, so getting this wrong does not degrade
        // an option, it takes the whole Feature install down when the env file is sourced.
        for (option, expected) in [
            ("plain", "PLAIN"),
            ("my-option", "MY_OPTION"),
            ("dotted.name", "DOTTED_NAME"),
            ("with space", "WITH_SPACE"),
            ("a--b", "A__B"),
            ("2fa", "_FA"),
            ("12ab", "_AB"),
            ("__dunder", "_DUNDER"),
            ("_1mixed", "_MIXED"),
            ("_leading", "_LEADING"),
        ] {
            assert_eq!(option_env_name(option), expected, "for option {option}");
        }
    }

    #[test]
    fn a_supplied_option_is_normalised_the_same_way_as_a_default() {
        let metadata = FeatureMetadata {
            options: BTreeMap::from([(
                "my-option".to_string(),
                OptionDef { default: Some(Value::String("a".into())) },
            )]),
            ..Default::default()
        };
        let supplied = BTreeMap::from([("other-one".to_string(), Value::String("b".into()))]);
        let resolved = resolve_options(&metadata, &supplied);
        assert_eq!(resolved.get("MY_OPTION").map(String::as_str), Some("a"));
        assert_eq!(resolved.get("OTHER_ONE").map(String::as_str), Some("b"));
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
        let reference = oci::parse_ref("ghcr.io/devcontainers/features/git:1");
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

    /// The two key lists, pinned against the reference CLI's own `pickConfigProperties` and
    /// `pickFeatureProperties`.
    ///
    /// These are transcriptions, not observations, and that is the point: a probe only shows
    /// the properties whichever Feature or config it was built from happened to declare. Both
    /// lists were wrong for exactly that reason — the Feature list was missing all five
    /// lifecycle hooks, and the config list was missing `shutdownAction`. Asserting the whole
    /// list, rather than a sample of it, is what makes a future omission fail here instead of
    /// silently dropping a property from every image `am` builds.
    #[test]
    fn the_key_lists_match_the_reference_implementation() {
        // src/spec-node/imageMetadata.ts — pickConfigProperties
        assert_eq!(
            CONFIG_METADATA_KEYS,
            [
                "onCreateCommand",
                "updateContentCommand",
                "postCreateCommand",
                "postStartCommand",
                "postAttachCommand",
                "waitFor",
                "customizations",
                "mounts",
                "containerEnv",
                "containerUser",
                "init",
                "privileged",
                "capAdd",
                "securityOpt",
                "remoteUser",
                "userEnvProbe",
                "remoteEnv",
                "overrideCommand",
                "portsAttributes",
                "otherPortsAttributes",
                "forwardPorts",
                "shutdownAction",
                "updateRemoteUserUID",
                "hostRequirements",
            ]
        );

        // pickFeatureProperties — the five lifecycle hooks lead, and there is no containerEnv:
        // a Feature's is baked into the image as ENV instead. See dockerfile::render.
        assert_eq!(
            FEATURE_METADATA_KEYS,
            [
                "onCreateCommand",
                "updateContentCommand",
                "postCreateCommand",
                "postStartCommand",
                "postAttachCommand",
                "init",
                "privileged",
                "capAdd",
                "securityOpt",
                "entrypoint",
                "mounts",
                "customizations",
            ]
        );
    }

    #[test]
    fn a_features_lifecycle_hooks_reach_the_label() {
        // Dropping these meant a Feature's own postCreateCommand never ran — `git-lfs` declares
        // one to pull its artifacts, so a checkout kept stub files.
        let raw: Value = serde_json_lenient::from_str(
            r#"{"postCreateCommand":"/usr/local/share/pull-git-lfs-artifacts.sh",
                "customizations":{"vscode":{}},"init":true}"#,
        )
        .unwrap();
        let keys: Vec<String> =
            metadata_properties(&raw, FEATURE_METADATA_KEYS).map(|(k, _)| k).collect();
        assert_eq!(keys, ["postCreateCommand", "init", "customizations"]);
    }

    #[test]
    fn a_config_emits_metadata_in_the_config_schema_order() {
        // Pinned against a real `devcontainer build`. The order is *not* the Feature order —
        // the CLI puts `customizations` seventh for a config and last for a Feature — and the
        // two were the same list here originally, so any config setting `customizations`
        // alongside `init` or `mounts` produced a label the CLI would not have written.
        let raw: Value = serde_json_lenient::from_str(
            r#"{"init":true,"customizations":{"vscode":{}},"forwardPorts":[3000],
                "mounts":[],"remoteUser":"root","postCreateCommand":"echo hi"}"#,
        )
        .unwrap();
        let snippet = config_label_snippet(&raw);
        let keys: Vec<&String> = snippet.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "postCreateCommand",
                "customizations",
                "mounts",
                "init",
                "remoteUser",
                "forwardPorts"
            ]
        );
    }

    #[test]
    fn a_feature_emits_metadata_in_the_feature_schema_order() {
        // Narrower than a config's, and a different order. `containerEnv` and `forwardPorts`
        // are dropped from a Feature entirely — verified against the reference CLI, which does
        // the same.
        let raw: Value = serde_json_lenient::from_str(
            r#"{"customizations":{"vscode":{}},"mounts":[],"entrypoint":"/e.sh",
                "capAdd":["SYS_PTRACE"],"init":true,"containerEnv":{"A":"1"},
                "forwardPorts":[3000]}"#,
        )
        .unwrap();
        let keys: Vec<String> =
            metadata_properties(&raw, FEATURE_METADATA_KEYS).map(|(k, _)| k).collect();
        assert_eq!(keys, ["init", "capAdd", "entrypoint", "mounts", "customizations"]);
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

    /// The four Features of the two-chains fixture, whose natural order is
    /// `gh-release, common-utils, act, git`.
    fn two_chains() -> Vec<ResolvedFeature> {
        vec![
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
        ]
    }

    #[test]
    fn an_override_reorders_within_a_round_but_cannot_jump_a_dependency() {
        // Pinned against `devcontainer features resolve-dependencies` for this exact config.
        // `git` is raised, but still waits for `common-utils` — priority is only consulted
        // among Features that are already eligible. What it does change is that `git` now
        // outranks `act` in the second round, where the natural order put `act` first.
        let ordered = super::install_order(
            two_chains(),
            &["ghcr.io/devcontainers/features/git".to_string()],
        )
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec![
                "ghcr.io/devcontainers-extra/features/gh-release:1",
                "ghcr.io/devcontainers/features/common-utils:2",
                "ghcr.io/devcontainers/features/git:1",
                "ghcr.io/devcontainers-extra/features/act:1",
            ]
        );
    }

    #[test]
    fn an_override_can_split_a_round_and_defer_the_rest() {
        // Also pinned against the reference CLI. Raising `common-utils` takes round one alone,
        // sending `gh-release` back to the worklist to compete again — which is the part of
        // the algorithm that a "sort by priority" shortcut would get wrong.
        let ordered = super::install_order(
            two_chains(),
            &["ghcr.io/devcontainers/features/common-utils".to_string()],
        )
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec![
                "ghcr.io/devcontainers/features/common-utils:2",
                "ghcr.io/devcontainers-extra/features/gh-release:1",
                "ghcr.io/devcontainers/features/git:1",
                "ghcr.io/devcontainers-extra/features/act:1",
            ]
        );
    }

    #[test]
    fn an_override_entry_may_be_the_short_alias() {
        // The CLI accepts `git` for `ghcr.io/devcontainers/features/git`, and the short form is
        // what people write.
        let long = super::install_order(
            two_chains(),
            &["ghcr.io/devcontainers/features/git".to_string()],
        )
        .unwrap();
        let short = super::install_order(two_chains(), &["git".to_string()]).unwrap();
        assert_eq!(ids(&long), ids(&short));
    }

    #[test]
    fn an_override_entry_matching_nothing_changes_no_order() {
        let plain = install_order(two_chains()).unwrap();
        let bogus = super::install_order(
            two_chains(),
            &["ghcr.io/nobody/features/absent".to_string()],
        )
        .unwrap();
        assert_eq!(ids(&plain), ids(&bogus));
    }

    #[test]
    fn earlier_override_entries_outrank_later_ones() {
        // Priority is `n - idx`, so the list reads first-installed to last-installed. Both are
        // eligible in the same round, so the list alone decides.
        let ordered = super::install_order(
            vec![
                feature("ghcr.io/x/features/apple:1", &[]),
                feature("ghcr.io/x/features/zebra:1", &[]),
            ],
            &["ghcr.io/x/features/zebra".to_string(), "ghcr.io/x/features/apple".to_string()],
        )
        .unwrap();
        assert_eq!(
            ids(&ordered),
            vec!["ghcr.io/x/features/zebra:1", "ghcr.io/x/features/apple:1"]
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
