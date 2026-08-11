//! `am`'s own devcontainer image builder.
//!
//! Covers the two cases that account for most real configs: a base image or Dockerfile with no
//! Features at all, and one with Features pulled from an OCI registry whose ordering is
//! expressible with `installsAfter` alone. Anything else is reported as [`Unsupported`] so the
//! caller can fall back to the reference CLI rather than build something subtly wrong.
//!
//! The contract with the rest of `am` is exactly one artifact: an image tagged `am-dc-<hash>`
//! carrying a `devcontainer.metadata` label. Everything downstream — [`super::merge`],
//! [`super::finalize`], the trust gate, the run path — is shared with the CLI builder and
//! cannot tell the two apart. That is what makes the differential test in `tests/` meaningful.

pub mod dockerfile;
pub mod feature;
pub mod oci;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use super::{BuildRequest, DevcontainerJson};
use crate::error::AmError;

/// A construct this builder does not implement. Carries its own explanation because the
/// message is user-facing: it is the line printed when `am` falls back to the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsupported {
    Compose,
    NoBase,
    DependsOn(String),
    OverrideInstallOrder,
    LocalFeature(String),
    TarballFeature(String),
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsupported::Compose => write!(f, "the config uses dockerComposeFile"),
            Unsupported::NoBase => {
                write!(f, "the config has neither an 'image' nor a 'build.dockerfile'")
            }
            Unsupported::DependsOn(id) => {
                write!(f, "Feature '{id}' uses dependsOn, which needs round-trip resolution")
            }
            Unsupported::OverrideInstallOrder => {
                write!(f, "the config sets overrideFeatureInstallOrder")
            }
            Unsupported::LocalFeature(id) => {
                write!(f, "Feature '{id}' is a local path rather than a registry reference")
            }
            Unsupported::TarballFeature(id) => {
                write!(f, "Feature '{id}' is a direct tarball rather than a registry reference")
            }
        }
    }
}

/// Everything the builder needs about one requested Feature before its manifest is fetched.
struct RequestedFeature {
    reference: oci::FeatureRef,
    options: BTreeMap<String, Value>,
}

/// Check the parts of a config that can be judged without touching the network.
///
/// Feature-level problems (`dependsOn`) can only be found after fetching manifests; those
/// surface later, from [`build`]. Cheap checks run first so an unsupported config falls back
/// before doing any I/O.
pub fn check_static(json: &DevcontainerJson, raw: &Value) -> Result<(), Unsupported> {
    if json.docker_compose_file.is_some() {
        return Err(Unsupported::Compose);
    }
    if json.image.is_none() && json.build.as_ref().and_then(|b| b.dockerfile.as_ref()).is_none() {
        return Err(Unsupported::NoBase);
    }
    if raw.get("overrideFeatureInstallOrder").is_some() {
        return Err(Unsupported::OverrideInstallOrder);
    }
    for id in requested_ids(raw) {
        match oci::parse_ref(&id) {
            oci::FeatureSource::Registry(_) => {}
            oci::FeatureSource::Local(_) => return Err(Unsupported::LocalFeature(id)),
            oci::FeatureSource::Tarball(_) => return Err(Unsupported::TarballFeature(id)),
        }
    }
    Ok(())
}

/// The Feature ids a `devcontainer.json` asks for, in declaration order.
fn requested_ids(raw: &Value) -> Vec<String> {
    raw.get("features")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// Collect the Features to install: the config's own, plus the ones `am` injects.
///
/// An injected Feature that the config already requests is *not* added twice — the config's
/// own options win, because a project that pinned an option meant it.
fn collect_features(raw: &Value, injected: &[super::InjectedFeature]) -> Vec<RequestedFeature> {
    let mut out: Vec<RequestedFeature> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    let mut push = |id: &str, options: BTreeMap<String, Value>, seen: &mut Vec<String>| {
        let oci::FeatureSource::Registry(reference) = oci::parse_ref(id) else {
            // check_static already rejected these; reaching here means a caller skipped it.
            return;
        };
        if seen.iter().any(|s| s == &reference.untagged()) {
            return;
        }
        seen.push(reference.untagged());
        out.push(RequestedFeature { reference, options });
    };

    if let Some(map) = raw.get("features").and_then(Value::as_object) {
        for (id, options) in map {
            push(id, as_option_map(options), &mut seen);
        }
    }
    for f in injected {
        let parsed: Value = serde_json_lenient::from_str(&f.options).unwrap_or(Value::Null);
        push(&f.id, as_option_map(&parsed), &mut seen);
    }
    out
}

/// Feature options are an object, but `{}` and a bare `"latest"` shorthand both appear.
fn as_option_map(value: &Value) -> BTreeMap<String, Value> {
    match value {
        Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        // The shorthand `"feature": "1.2.3"` means the `version` option.
        Value::String(s) => BTreeMap::from([("version".to_string(), Value::String(s.clone()))]),
        _ => BTreeMap::new(),
    }
}

// ── Base image ────────────────────────────────────────────────────────────────

/// Resolve the image the Features get installed on top of.
///
/// A `build.dockerfile` config is built first, into its own tag, so that the Feature-install
/// stage always has a concrete image to start `FROM`. This is the same shape the CLI uses
/// (`_DEV_CONTAINERS_BASE_IMAGE`) and it keeps the generated Dockerfile identical in both cases.
fn resolve_base(req: &BuildRequest, runtime_bin: &Path) -> Result<String> {
    if let Some(image) = &req.json.image {
        ensure_present(runtime_bin, image)?;
        return Ok(image.clone());
    }

    let dockerfile = super::dockerfile_path(req.config_path, req.json)
        .ok_or_else(|| AmError::DevcontainerBuildFailed("no Dockerfile to build".into()))?;
    let context = req
        .json
        .build
        .as_ref()
        .and_then(|b| b.context.as_deref())
        .map(|c| req.config_path.parent().unwrap_or(Path::new(".")).join(c))
        .unwrap_or_else(|| {
            dockerfile
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| req.worktree.to_path_buf())
        });

    let tag = format!("{}-base", req.image);
    let mut args = vec![
        "build".to_string(),
        "-f".to_string(),
        dockerfile.to_string_lossy().into_owned(),
        "-t".to_string(),
        tag.clone(),
    ];
    if let Some(build) = &req.json.build {
        for (key, value) in &build.args {
            args.push("--build-arg".to_string());
            args.push(format!("{key}={}", render_build_arg(value)));
        }
        if let Some(target) = &build.target {
            args.push("--target".to_string());
            args.push(target.clone());
        }
    }
    if req.no_cache {
        args.push("--no-cache".to_string());
    }
    args.push(context.to_string_lossy().into_owned());

    run_build(runtime_bin, &args, "building the devcontainer's Dockerfile")?;
    Ok(tag)
}

fn render_build_arg(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Pull the base image if it is not already local, so its metadata can be inspected.
fn ensure_present(runtime_bin: &Path, image: &str) -> Result<()> {
    if super::image_exists(runtime_bin, image) {
        return Ok(());
    }
    let status = std::process::Command::new(runtime_bin)
        .args(["pull", image])
        .status()
        .map_err(|e| AmError::DevcontainerBuildFailed(format!("failed to run pull: {e}")))?;
    if !status.success() {
        return Err(
            AmError::DevcontainerBuildFailed(format!("could not pull base image {image}")).into(),
        );
    }
    Ok(())
}

/// The base image's own metadata label, as raw JSON elements.
///
/// Kept as `Value` rather than parsed into `MetadataSnippet` so that properties `am` does not
/// model survive into the rebuilt label untouched — a base image built by someone else's
/// tooling must not lose information by passing through here.
fn base_label_elements(runtime_bin: &Path, image: &str) -> Result<Vec<Value>> {
    let output = std::process::Command::new(runtime_bin)
        .args([
            "inspect",
            image,
            "--format",
            "{{ index .Config.Labels \"devcontainer.metadata\" }}",
        ])
        .output()
        .map_err(|e| AmError::DevcontainerBuildFailed(format!("failed to inspect {image}: {e}")))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "<no value>" {
        return Ok(Vec::new());
    }
    match serde_json_lenient::from_str::<Value>(trimmed) {
        Ok(Value::Array(items)) => Ok(items),
        Ok(Value::Object(o)) => Ok(vec![Value::Object(o)]),
        _ => Ok(Vec::new()),
    }
}

/// The user the base image runs as, which the final `USER` instruction restores.
fn base_image_user(runtime_bin: &Path, image: &str) -> String {
    let output = std::process::Command::new(runtime_bin)
        .args(["inspect", image, "--format", "{{ .Config.User }}"])
        .output();
    let user = output
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if user.is_empty() || user == "<no value>" {
        "root".to_string()
    } else {
        user
    }
}

// ── Label composition ─────────────────────────────────────────────────────────

/// Assemble the `devcontainer.metadata` label.
///
/// Order *is* precedence: the base image's own elements first, then each Feature in install
/// order, then the `devcontainer.json` contribution last. [`super::merge`] folds left to right,
/// so this is what makes the config win over a Feature and a Feature win over the base image.
pub fn compose_label(
    base: &[Value],
    features: &[feature::ResolvedFeature],
    config: &Value,
) -> String {
    let mut elements: Vec<Value> = base.to_vec();
    elements.extend(features.iter().map(feature::ResolvedFeature::label_snippet));

    let config_snippet = feature::config_label_snippet(config);
    // An empty object carries nothing and only makes the label harder to read.
    if config_snippet.as_object().is_some_and(|o| !o.is_empty()) {
        elements.push(config_snippet);
    }

    let rendered: Vec<String> = elements.iter().map(Value::to_string).collect();
    // Spacing matches the reference CLI's output so a byte comparison against a
    // CLI-built image is a meaningful test rather than a whitespace diff.
    format!("[ {} ]", rendered.join(", "))
}

// ── Build ─────────────────────────────────────────────────────────────────────

/// Build the image, or report why this builder cannot.
///
/// Returns `Ok(Err(unsupported))` when the config is valid but out of scope — a fallback
/// signal, not a failure. `Err(_)` means the build itself went wrong and falling back would
/// just repeat it.
pub fn build(
    req: &BuildRequest,
    runtime_bin: &Path,
    raw_config: &Value,
) -> Result<Result<String, Unsupported>> {
    if let Err(reason) = check_static(req.json, raw_config) {
        return Ok(Err(reason));
    }

    let requested = collect_features(raw_config, req.injected);

    // Manifests first: they carry each Feature's metadata, so ordering and options resolve
    // before any layer is downloaded — and a dependsOn fallback costs three small GETs.
    let mut resolved = Vec::with_capacity(requested.len());
    for item in &requested {
        let manifest = oci::fetch_manifest(&item.reference)?;
        let metadata_text = manifest.feature_metadata().ok_or_else(|| {
            AmError::DevcontainerBuildFailed(format!(
                "{} is not a devcontainer Feature (its manifest carries no metadata)",
                item.reference.raw
            ))
        })?;
        let (metadata, raw) = feature::parse_metadata(metadata_text)?;
        if metadata.depends_on.is_some() {
            return Ok(Err(Unsupported::DependsOn(item.reference.raw.clone())));
        }
        let options = feature::resolve_options(&metadata, &item.options);
        let layer = manifest
            .feature_layer()
            .ok_or_else(|| {
                AmError::DevcontainerBuildFailed(format!(
                    "{} has no layer to install",
                    item.reference.raw
                ))
            })?
            .clone();
        resolved.push((
            feature::ResolvedFeature {
                reference: item.reference.clone(),
                metadata,
                raw,
                options,
            },
            layer,
        ));
    }

    // Ordering moves the Features around, so the layer to download is looked up by id
    // afterwards rather than carried along positionally.
    let layers: BTreeMap<String, oci::Layer> = resolved
        .iter()
        .map(|(f, layer)| (f.reference.raw.clone(), layer.clone()))
        .collect();
    let ordered = feature::install_order(resolved.into_iter().map(|(f, _)| f).collect())?;

    let base = resolve_base(req, runtime_bin)?;
    let base_elements = base_label_elements(runtime_bin, &base)?;
    let image_user = base_image_user(runtime_bin, &base);

    // The two users the install contract exposes, resolved across the whole precedence chain.
    let container_user = effective_user(&base_elements, raw_config, "containerUser")
        .unwrap_or_else(|| image_user.clone());
    let remote_user = effective_user(&base_elements, raw_config, "remoteUser")
        .unwrap_or_else(|| container_user.clone());

    let cache_root = oci::cache_root();
    let mut cached_dirs = Vec::with_capacity(ordered.len());
    for f in &ordered {
        let layer = layers.get(&f.reference.raw).ok_or_else(|| {
            AmError::DevcontainerBuildFailed(format!("lost the layer for {}", f.reference.raw))
        })?;
        cached_dirs.push(oci::fetch_layer(&f.reference, layer, &cache_root)?);
    }

    let label = compose_label(&base_elements, &ordered, raw_config);

    // Deliberately not a temp dir that vanishes: when a Feature's install.sh fails, the
    // generated Dockerfile and the exact staged inputs are the first things worth looking at.
    let staging = staging_dir(req.image)?;
    let context = staging.join("context");
    dockerfile::write_context(&context, &ordered, &cached_dirs, &container_user, &remote_user)?;

    // The Dockerfile lives beside the context, not in it, so it does not end up copied into
    // the image by the `COPY .` that stages the Features.
    let dockerfile_path = staging.join("Dockerfile");
    std::fs::write(
        &dockerfile_path,
        dockerfile::render(&ordered, &label, &container_user, &remote_user),
    )
    .context("writing the generated Dockerfile")?;

    let mut args = vec![
        "build".to_string(),
        "--build-arg".to_string(),
        format!("_DEV_CONTAINERS_BASE_IMAGE={base}"),
        "--build-arg".to_string(),
        format!("_DEV_CONTAINERS_IMAGE_USER={image_user}"),
        "--target".to_string(),
        "dev_containers_target_stage".to_string(),
        "-f".to_string(),
        dockerfile_path.to_string_lossy().into_owned(),
        "-t".to_string(),
        req.image.to_string(),
    ];
    if req.no_cache {
        args.push("--no-cache".to_string());
    }
    args.push(context.to_string_lossy().into_owned());

    run_build(runtime_bin, &args, "installing devcontainer Features")?;
    Ok(Ok(req.image.to_string()))
}

/// Resolve a user property across the precedence chain: base image, then `devcontainer.json`.
///
/// Features may also set these, but a Feature that renames the user it is being installed for
/// would be self-defeating, and the reference CLI resolves the build-time value the same way.
fn effective_user(base: &[Value], config: &Value, key: &str) -> Option<String> {
    config
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            base.iter()
                .rev()
                .find_map(|e| e.get(key).and_then(Value::as_str).map(str::to_string))
        })
}

/// A per-image staging area for the generated build inputs, cleared before each build.
///
/// Clearing matters: a stale Feature directory left by a previous config would otherwise be
/// picked up by the `COPY .` that stages the build context.
fn staging_dir(image: &str) -> Result<std::path::PathBuf> {
    let sanitized: String = image
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    let dir = oci::cache_root()
        .parent()
        .map(|p| p.join("build"))
        .unwrap_or_else(|| std::path::PathBuf::from(".am-build"))
        .join(sanitized);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing the staging area {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating the staging area {}", dir.display()))?;
    Ok(dir)
}

/// Run a build, letting its output through to the terminal.
///
/// A Feature install can take minutes; capturing the stream would make it look like a hang.
fn run_build(runtime_bin: &Path, args: &[String], what: &str) -> Result<()> {
    let status = std::process::Command::new(runtime_bin)
        .args(args)
        .status()
        .map_err(|e| {
            AmError::DevcontainerBuildFailed(format!(
                "failed to run {}: {e}",
                runtime_bin.display()
            ))
        })?;
    if !status.success() {
        return Err(AmError::DevcontainerBuildFailed(format!(
            "{what} failed ({status}) — see the output above"
        ))
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(text: &str) -> (DevcontainerJson, Value) {
        (
            serde_json_lenient::from_str(text).unwrap(),
            serde_json_lenient::from_str(text).unwrap(),
        )
    }

    #[test]
    fn a_plain_image_config_is_supported() {
        let (typed, raw) = json(r#"{"image":"debian:bookworm-slim"}"#);
        assert_eq!(check_static(&typed, &raw), Ok(()));
    }

    #[test]
    fn a_registry_feature_config_is_supported() {
        let (typed, raw) = json(
            r#"{"image":"debian","features":{"ghcr.io/devcontainers/features/git:1":{}}}"#,
        );
        assert_eq!(check_static(&typed, &raw), Ok(()));
    }

    #[test]
    fn compose_falls_back() {
        let (typed, raw) = json(r#"{"dockerComposeFile":"docker-compose.yml"}"#);
        assert_eq!(check_static(&typed, &raw), Err(Unsupported::Compose));
    }

    #[test]
    fn a_config_with_no_base_falls_back() {
        let (typed, raw) = json(r#"{"name":"nothing"}"#);
        assert_eq!(check_static(&typed, &raw), Err(Unsupported::NoBase));
    }

    #[test]
    fn override_install_order_falls_back() {
        let (typed, raw) = json(
            r#"{"image":"debian","overrideFeatureInstallOrder":["ghcr.io/devcontainers/features/git"]}"#,
        );
        assert_eq!(check_static(&typed, &raw), Err(Unsupported::OverrideInstallOrder));
    }

    #[test]
    fn a_local_feature_falls_back() {
        let (typed, raw) = json(r#"{"image":"debian","features":{"./local-feature":{}}}"#);
        assert_eq!(
            check_static(&typed, &raw),
            Err(Unsupported::LocalFeature("./local-feature".to_string()))
        );
    }

    #[test]
    fn a_tarball_feature_falls_back() {
        let (typed, raw) =
            json(r#"{"image":"debian","features":{"https://example.com/f.tgz":{}}}"#);
        assert!(matches!(
            check_static(&typed, &raw),
            Err(Unsupported::TarballFeature(_))
        ));
    }

    #[test]
    fn fallback_reasons_name_the_offending_construct() {
        // These strings are printed to the user, so they must identify what to look at.
        assert!(Unsupported::DependsOn("ghcr.io/x/y:1".into())
            .to_string()
            .contains("ghcr.io/x/y:1"));
        assert!(Unsupported::Compose.to_string().contains("dockerComposeFile"));
    }

    #[test]
    fn injected_features_are_added_to_the_config_s_own() {
        let (_, raw) = json(r#"{"image":"debian","features":{"ghcr.io/a/b/git:1":{}}}"#);
        let injected = [super::super::InjectedFeature::with_defaults(
            "ghcr.io/anthropics/devcontainer-features/claude-code:1",
        )];
        let collected = collect_features(&raw, &injected);
        assert_eq!(collected.len(), 2);
    }

    #[test]
    fn an_injected_feature_the_config_already_pins_is_not_duplicated() {
        let (_, raw) = json(
            r#"{"image":"debian","features":{"ghcr.io/anthropics/devcontainer-features/claude-code:1":{"version":"pinned"}}}"#,
        );
        let injected = [super::super::InjectedFeature::with_defaults(
            "ghcr.io/anthropics/devcontainer-features/claude-code:1",
        )];
        let collected = collect_features(&raw, &injected);
        assert_eq!(collected.len(), 1);
        // The config's own options survive rather than being reset to the injected defaults.
        assert_eq!(
            collected[0].options.get("version").and_then(Value::as_str),
            Some("pinned")
        );
    }

    #[test]
    fn the_version_shorthand_becomes_a_version_option() {
        let (_, raw) = json(r#"{"image":"debian","features":{"ghcr.io/a/b/node:1":"20"}}"#);
        let collected = collect_features(&raw, &[]);
        assert_eq!(
            collected[0].options.get("version").and_then(Value::as_str),
            Some("20")
        );
    }

    #[test]
    fn label_precedence_runs_base_then_features_then_config() {
        let base = vec![serde_json::json!({"remoteUser": "vscode"})];
        let (_, raw) = json(r#"{"image":"debian","remoteUser":"root"}"#);
        let label = compose_label(&base, &[], &raw);
        let parsed: Vec<Value> = serde_json::from_str(&label).unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["remoteUser"], "vscode");
        // The config is last, so merge() lets it win.
        assert_eq!(parsed[1]["remoteUser"], "root");
    }

    #[test]
    fn a_config_contributing_nothing_adds_no_element() {
        let (_, raw) = json(r#"{"image":"debian","name":"demo"}"#);
        let label = compose_label(&[], &[], &raw);
        assert_eq!(label, "[  ]");
    }

    #[test]
    fn composed_label_round_trips_through_the_shared_parser() {
        // The whole point of the builder: whatever it writes, the existing run path reads.
        let base = vec![serde_json::json!({"remoteUser": "vscode", "init": true})];
        let (_, raw) = json(r#"{"image":"debian","containerEnv":{"FOO":"bar"}}"#);
        let label = compose_label(&base, &[], &raw);

        let snippets = super::super::parse_metadata_label(&label).unwrap();
        let merged = super::super::merge(&snippets).unwrap();
        assert!(merged.init);
        assert_eq!(merged.remote_user.as_deref(), Some("vscode"));
        assert_eq!(merged.container_env.get("FOO").map(String::as_str), Some("bar"));
    }

    #[test]
    fn effective_user_prefers_the_config_over_the_base_image() {
        let base = vec![serde_json::json!({"remoteUser": "vscode"})];
        let (_, raw) = json(r#"{"image":"debian","remoteUser":"root"}"#);
        assert_eq!(
            effective_user(&base, &raw, "remoteUser"),
            Some("root".to_string())
        );
    }

    /// End-to-end differential check against the reference CLI.
    ///
    /// Ignored by default: it needs a container runtime, network access to ghcr.io, and a few
    /// minutes. Run it with `cargo test -- --ignored --nocapture` when touching the builder,
    /// and regenerate `cli-git-label.json` from a real `devcontainer build` if the CLI's label
    /// format ever changes.
    ///
    /// The assertion is not "the images are identical" — they are not, and do not need to be.
    /// It is that the *label* is, because the label is the entire contract between whichever
    /// builder ran and everything downstream of it.
    #[test]
    #[ignore = "needs a container runtime and network access"]
    fn native_build_label_matches_the_reference_cli() {
        assert_matches_reference(
            include_str!("../../../tests/fixtures/devcontainer/native/git-devcontainer.json"),
            include_str!("../../../tests/fixtures/devcontainer/native/cli-git-label.json"),
            "am-dc-native-difftest",
            Some(("git", "--version", "git version")),
        );
    }

    /// The harder case: a base image that already carries its own metadata label, two Features
    /// with an `installsAfter` relationship between them, and a `${localWorkspaceFolder}` that
    /// must survive Docker's own variable expansion to be substituted later by the run path.
    #[test]
    #[ignore = "needs a container runtime and network access"]
    fn native_build_matches_the_cli_with_features_and_an_inherited_label() {
        assert_matches_reference(
            include_str!("../../../tests/fixtures/devcontainer/native/features-devcontainer.json"),
            include_str!("../../../tests/fixtures/devcontainer/native/cli-features-label.json"),
            "am-dc-native-difftest-features",
            None,
        );
    }

    /// Build `config_text` with `am`'s own builder and assert the resulting metadata label is
    /// byte-identical to what the reference CLI produced for the same config.
    fn assert_matches_reference(
        config_text: &str,
        reference_label: &str,
        image: &str,
        probe: Option<(&str, &str, &str)>,
    ) {
        let runtime = std::path::PathBuf::from(
            std::env::var("AM_DOCKER_BIN").unwrap_or_else(|_| "/usr/bin/docker".to_string()),
        );

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("devcontainer.json");
        std::fs::write(&config_path, config_text).unwrap();

        let typed: DevcontainerJson = serde_json_lenient::from_str(config_text).unwrap();
        let raw: Value = serde_json_lenient::from_str(config_text).unwrap();

        let built = build(
            &BuildRequest {
                worktree: tmp.path(),
                config_path: &config_path,
                json: &typed,
                image,
                injected: &[],
                // Layer caching makes a re-run fast but proves less: set AM_TEST_NO_CACHE=1
                // to force a cold build when verifying the builder end to end.
                no_cache: std::env::var("AM_TEST_NO_CACHE").is_ok(),
            },
            &runtime,
            &raw,
        )
        .expect("build succeeds")
        .expect("config is supported natively");
        assert_eq!(built, image);

        let ours = String::from_utf8(
            std::process::Command::new(&runtime)
                .args([
                    "inspect",
                    image,
                    "--format",
                    "{{ index .Config.Labels \"devcontainer.metadata\" }}",
                ])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let ours = ours.trim();
        let theirs = reference_label.trim();

        assert_eq!(
            ours, theirs,
            "\n am: {ours}\ncli: {theirs}\nthe metadata label must match the reference CLI byte for byte"
        );

        // And the thing that actually matters: both labels resolve to the same run config.
        let mine = super::super::merge(&super::super::parse_metadata_label(ours).unwrap()).unwrap();
        let reference =
            super::super::merge(&super::super::parse_metadata_label(theirs).unwrap()).unwrap();
        assert_eq!(mine, reference);

        // Prove the Feature actually installed, rather than the label merely being right.
        if let Some((bin, arg, expected)) = probe {
            let out = std::process::Command::new(&runtime)
                .args(["run", "--rm", image, bin, arg])
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&out.stdout).contains(expected),
                "'{bin} {arg}' should report '{expected}' — the Feature did not install"
            );
        }
    }

    #[test]
    fn effective_user_falls_through_to_the_base_image() {
        let base = vec![serde_json::json!({"remoteUser": "vscode"})];
        let (_, raw) = json(r#"{"image":"debian"}"#);
        assert_eq!(
            effective_user(&base, &raw, "remoteUser"),
            Some("vscode".to_string())
        );
    }
}
