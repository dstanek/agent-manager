//! `am`'s own devcontainer image builder.
//!
//! Covers every shape the spec defines: a base `image`, a `build.dockerfile`, or a
//! `dockerComposeFile`, plus Features from an OCI registry, a path in the repo, or a tarball
//! URL — ordered by `dependsOn`, `installsAfter`, and `overrideFeatureInstallOrder`. There is no
//! construct left that sends `am` to the reference CLI; a config this cannot build is one the
//! reference CLI rejects too, and it is reported as the error it is.
//!
//! The contract with the rest of `am` is exactly one artifact: an image tagged `am-dc-<hash>`
//! carrying a `devcontainer.metadata` label. Everything downstream — [`super::merge`],
//! [`super::finalize`], the trust gate, the run path — is shared with the CLI builder and
//! cannot tell the two apart. That is what makes the differential test in `tests/` meaningful.

pub mod auth;
pub mod dockerfile;
pub mod feature;
pub mod oci;
/// An OCI registry that runs inside the test process — see the module's own docs for what it
/// replaces and, more importantly, what it deliberately does not.
#[cfg(test)]
mod test_registry;

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use super::{BuildRequest, DevcontainerJson};
use crate::error::AmError;

/// Everything the builder needs about one requested Feature before its manifest is fetched.
struct RequestedFeature {
    reference: oci::FeatureRef,
    options: BTreeMap<String, Value>,
}

/// Check the parts of a config that can be judged without touching the network.
///
/// There is exactly one: whether the config says what to build at all. This is not a limitation
/// of `am`'s builder — the reference CLI rejects the same configs with "No image information
/// specified in devcontainer.json", and `build.dockerfile` has no default there either — so it
/// is an error rather than a reason to go asking a second tool the same unanswerable question.
pub fn check_static(json: &DevcontainerJson) -> Result<()> {
    let has_base = json.image.is_some()
        || json.build.as_ref().and_then(|b| b.dockerfile.as_ref()).is_some()
        // A compose config's base is the agent service's own image or build section, which
        // only the runtime can resolve — so this cannot be judged any further from here.
        || json.docker_compose_file.is_some();
    if !has_base {
        return Err(AmError::ConfigError(
            "this devcontainer.json has nothing to build from\n\
             Add an \"image\", a \"build\" with a \"dockerfile\", or a \"dockerComposeFile\""
                .to_string(),
        )
        .into());
    }
    Ok(())
}

/// `overrideFeatureInstallOrder`, or empty. Non-string entries are dropped rather than rejected:
/// the property is advisory, and a malformed entry cannot match a Feature anyway.
fn override_install_order(raw: &Value) -> Vec<String> {
    raw.get("overrideFeatureInstallOrder")
        .and_then(Value::as_array)
        .map(|entries| {
            entries.iter().filter_map(Value::as_str).map(str::to_string).collect()
        })
        .unwrap_or_default()
}

/// Collect the Features to install: the config's own, plus the ones `am` injects.
///
/// An injected Feature that the config already requests is *not* added twice — the config's
/// own options win, because a project that pinned an option meant it.
fn collect_features(raw: &Value, injected: &[super::InjectedFeature]) -> Vec<RequestedFeature> {
    let mut out: Vec<RequestedFeature> = Vec::new();
    let mut seen: Vec<String> = Vec::new();

    // The config's own Features are taken as written. Two *tags* of one Feature are two
    // Features per the spec's equality rule — `{"…/node:18": {}, "…/node:20": {}}` asks for
    // both — so deduplication here is by the full id, not by the untagged name.
    if let Some(map) = raw.get("features").and_then(Value::as_object) {
        for (id, options) in map {
            let reference = oci::parse_ref(id);
            if seen.iter().any(|s| s == id) {
                continue;
            }
            seen.push(id.clone());
            out.push(RequestedFeature { reference, options: as_option_map(options) });
        }
    }

    // An injected Feature defers to the config on the *untagged* name: a project that pinned
    // its own version of what `am` injects meant that version, whichever tag it chose.
    for f in injected {
        let reference = oci::parse_ref(&f.id);
        let untagged = reference.untagged();
        if out.iter().any(|r| r.reference.untagged() == untagged) {
            continue;
        }
        let parsed: Value = serde_json_lenient::from_str(&f.options).unwrap_or(Value::Null);
        out.push(RequestedFeature { reference, options: as_option_map(&parsed) });
    }
    out
}

/// Resolve every Feature that will be installed: the requested set, plus everything their
/// `dependsOn` pulls in, transitively.
///
/// Manifests come first and layers are downloaded later, which is what makes the round trip
/// cheap: a manifest carries the whole `devcontainer-feature.json` in an annotation, so walking
/// the dependency graph costs one small GET per node and downloads nothing.
///
/// Two Features are the same node when their contents and options match, so a diamond — two
/// Features depending on the same third — installs it once, and a `dependsOn` cycle terminates
/// instead of looping. The same Feature requested with *different* options is genuinely two
/// nodes, and the spec says to install both.
fn resolve_graph(
    requested: Vec<RequestedFeature>,
    config_dir: &Path,
    lock: &super::lock::Lockfile,
    resolved_lock: &mut super::lock::Lockfile,
) -> Result<Vec<(feature::ResolvedFeature, Content)>> {
    let cache_root = oci::cache_root();
    let mut worklist: std::collections::VecDeque<RequestedFeature> = requested.into();
    let mut nodes: Vec<(feature::ResolvedFeature, Content)> = Vec::new();
    // What a caller asked for → the node it got. A parent links to its dependencies through
    // this, so linking never needs to re-derive a digest it already fetched.
    let mut by_request: BTreeMap<String, usize> = BTreeMap::new();
    // Contents + options → node, the spec's Feature-equality rule.
    let mut by_identity: BTreeMap<String, usize> = BTreeMap::new();
    // Deferred because a dependency may still be in the worklist when its parent is resolved.
    let mut pending_links: Vec<(usize, Vec<String>)> = Vec::new();

    while let Some(item) = worklist.pop_front() {
        let request = feature::request_key(&item.reference.raw, &item.options);
        if by_request.contains_key(&request) {
            continue;
        }

        let (metadata, raw, digest, content) =
            fetch_feature(&item.reference, config_dir, &cache_root, lock)?;
        record_in_lock(resolved_lock, &item.reference, &metadata, &digest);

        let identity = feature::request_key(&digest, &item.options);
        if let Some(&existing) = by_identity.get(&identity) {
            // A different id for contents already staged — a moving tag and its pinned version,
            // say. One install, and both requests point at it.
            by_request.insert(request, existing);
            continue;
        }

        let mut deps = Vec::new();
        for (id, options) in depends_on_entries(&metadata) {
            deps.push(feature::request_key(&id, &options));
            worklist.push_back(RequestedFeature {
                reference: oci::parse_ref(&id),
                options,
            });
        }

        let index = nodes.len();
        by_request.insert(request, index);
        by_identity.insert(identity, index);
        if !deps.is_empty() {
            pending_links.push((index, deps));
        }
        nodes.push((
            feature::ResolvedFeature {
                reference: item.reference.clone(),
                options: feature::resolve_options(&metadata, &item.options),
                metadata,
                raw,
                supplied: item.options,
                digest,
                hard_deps: Vec::new(),
            },
            content,
        ));
    }

    for (index, deps) in pending_links {
        let keys = deps
            .iter()
            .filter_map(|request| by_request.get(request))
            .map(|&i| nodes[i].0.key())
            .collect();
        nodes[index].0.hard_deps = keys;
    }
    Ok(nodes)
}

/// Where a resolved Feature's files will come from when the build context is written.
///
/// A registry layer is deliberately *not* downloaded during resolution — the manifest already
/// carries everything ordering needs — so it stays a reference until the order is settled. The
/// other two have to be materialised to be read at all, so by then they are already directories.
#[derive(Clone, Debug)]
enum Content {
    Layer(oci::Layer),
    Directory(std::path::PathBuf),
}

/// Read a Feature's metadata, and say where its files are.
///
/// Returns the parsed metadata, the raw JSON behind it, the digest that identifies the Feature,
/// and its content. The digest differs per kind by necessity: a registry Feature has an
/// immutable layer digest, a tarball is hashed from the bytes fetched, and a local Feature has
/// no content hash at all — the spec says every local Feature is distinct, so its resolved path
/// *is* its identity.
fn fetch_feature(
    reference: &oci::FeatureRef,
    config_dir: &Path,
    cache_root: &Path,
    lock: &super::lock::Lockfile,
) -> Result<(feature::FeatureMetadata, Value, String, Content)> {
    match reference.kind {
        oci::FeatureKind::Registry => {
            // A pinned digest wins over the written tag: that is the whole point of the
            // lockfile, and it is why two people building the same config get the same Feature.
            let pinned = lock.digest_for(&reference.raw).map(|digest| oci::FeatureRef {
                digest: Some(digest.to_string()),
                ..reference.clone()
            });
            let reference = pinned.as_ref().unwrap_or(reference);
            let manifest = oci::fetch_manifest(reference)?;
            let text = manifest.feature_metadata().ok_or_else(|| {
                AmError::DevcontainerBuildFailed(format!(
                    "{} is not a devcontainer Feature (its manifest carries no metadata)",
                    reference.raw
                ))
            })?;
            let (metadata, raw) = feature::parse_metadata(text)?;
            let layer = manifest
                .feature_layer()
                .ok_or_else(|| {
                    AmError::DevcontainerBuildFailed(format!(
                        "{} has no layer to install",
                        reference.raw
                    ))
                })?
                .clone();
            Ok((metadata, raw, manifest.digest.clone(), Content::Layer(layer)))
        }
        oci::FeatureKind::Local => {
            // Relative to the directory holding the devcontainer.json, which is what the
            // reference CLI resolves against — including for a path reached through dependsOn.
            let dir = config_dir.join(&reference.raw);
            let (metadata, raw) = read_feature_dir(&dir, &reference.raw)?;
            let canonical = dir.canonicalize().unwrap_or(dir.clone());
            Ok((
                metadata,
                raw,
                format!("local:{}", canonical.display()),
                Content::Directory(dir),
            ))
        }
        oci::FeatureKind::Tarball => {
            let (dir, digest) = oci::fetch_tarball(reference, cache_root)?;
            // A tarball URL is mutable and carries no digest of its own, so the lockfile's
            // integrity hash is the only thing that can tell you the bytes changed underneath
            // you. Refusing is the point: silently installing different code than the lockfile
            // records would make the file worse than useless.
            if let Some(expected) = lock.integrity_for(&reference.raw) {
                if expected != digest {
                    return Err(AmError::DevcontainerBuildFailed(format!(
                        "Feature '{}' does not match the lockfile\n\
                         expected {expected}, got {digest}\n\
                         The tarball changed at its URL. Delete its devcontainer-lock.json \
                         entry to accept the new contents.",
                        reference.raw
                    ))
                    .into());
                }
            }
            let (metadata, raw) = read_feature_dir(&dir, &reference.raw)?;
            Ok((metadata, raw, digest, Content::Directory(dir)))
        }
    }
}

/// Record what a Feature resolved to, for the lockfile `am` writes back.
///
/// Local Features are skipped: the spec excludes them, and `am` hashes their files directly,
/// which is both cheaper and exact.
fn record_in_lock(
    lock: &mut super::lock::Lockfile,
    reference: &oci::FeatureRef,
    metadata: &feature::FeatureMetadata,
    digest: &str,
) {
    let resolved = match reference.kind {
        oci::FeatureKind::Registry => format!("{}@{digest}", reference.untagged()),
        oci::FeatureKind::Tarball => reference.raw.clone(),
        oci::FeatureKind::Local => return,
    };
    let mut depends_on: Vec<String> = depends_on_entries(metadata)
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    depends_on.sort();
    lock.insert(
        &reference.raw,
        super::lock::LockEntry {
            version: metadata.version.clone(),
            resolved,
            integrity: digest.to_string(),
            depends_on,
        },
    );
}

/// Read the `devcontainer-feature.json` out of an unpacked Feature directory.
fn read_feature_dir(dir: &Path, id: &str) -> Result<(feature::FeatureMetadata, Value)> {
    let path = dir.join("devcontainer-feature.json");
    let text = std::fs::read_to_string(&path).map_err(|e| {
        AmError::DevcontainerBuildFailed(format!(
            "{id} has no devcontainer-feature.json at {}: {e}",
            path.display()
        ))
    })?;
    feature::parse_metadata(&text)
}

/// The `dependsOn` map as (id, options) pairs. Absent, null, or non-object all mean none.
fn depends_on_entries(metadata: &feature::FeatureMetadata) -> Vec<(String, BTreeMap<String, Value>)> {
    metadata
        .depends_on
        .as_ref()
        .and_then(Value::as_object)
        .map(|map| map.iter().map(|(id, o)| (id.clone(), as_option_map(o))).collect())
        .unwrap_or_default()
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
    if req.json.docker_compose_file.is_some() {
        return compose_base(req, runtime_bin);
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
        // Build args are substituted: the spec allows `${localWorkspaceFolder}` and friends
        // here, and passing one through literally bakes the unexpanded string into the image.
        let ctx = super::SubstitutionContext::new(
            req.worktree,
            &req.json
                .workspace_folder
                .clone()
                .unwrap_or_else(|| req.worktree.to_string_lossy().into_owned()),
        );
        for (key, value) in &build.args {
            args.push("--build-arg".to_string());
            args.push(format!("{key}={}", ctx.substitute(&render_build_arg(value))));
        }
        if let Some(target) = &build.target {
            args.push("--target".to_string());
            args.push(target.clone());
        }
        for image in build.cache_from.images() {
            args.push("--cache-from".to_string());
            args.push(image);
        }
        // Passed through verbatim, which is the point of the property: it is the escape hatch
        // for a runtime flag `am` does not model.
        args.extend(build.options.iter().cloned());
    }
    if req.no_cache {
        args.push("--no-cache".to_string());
    }
    args.push(context.to_string_lossy().into_owned());

    run_build(runtime_bin, &args, "building the devcontainer's Dockerfile")?;
    Ok(tag)
}

/// The base image for a compose config: whatever the agent's service is defined with.
///
/// Features are installed on top of that one service's image and nothing else. The rest of the
/// project is the repo's business — `am` is providing an environment for the agent, not taking
/// ownership of a database's image.
fn compose_base(req: &BuildRequest, runtime_bin: &Path) -> Result<String> {
    let service = req.json.service.as_deref().ok_or_else(|| {
        AmError::DevcontainerBuildFailed(
            "a compose devcontainer must name the service to build".into(),
        )
    })?;
    let files = super::compose_files(req.config_path, req.json);
    let resolved = crate::compose::resolved_config(runtime_bin, &files)?;
    let definition = crate::compose::service_definition(&resolved, service)?;

    if let Some(image) = definition.image {
        ensure_present(runtime_bin, &image)?;
        return Ok(image);
    }

    // A service defined by `build:` is built first, exactly as a non-compose `build.dockerfile`
    // config is — compose has already resolved the context and dockerfile to absolute paths.
    let (context, dockerfile) = definition.build.expect("service_definition guarantees one");
    let tag = format!("{}-base", req.image);
    let mut args = vec!["build".to_string(), "-t".to_string(), tag.clone()];
    if let Some(file) = dockerfile {
        args.push("-f".to_string());
        args.push(context.join(file).to_string_lossy().into_owned());
    }
    if req.no_cache {
        args.push("--no-cache".to_string());
    }
    args.push(context.to_string_lossy().into_owned());
    run_build(runtime_bin, &args, "building the compose service's image")?;
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
/// Every error here is terminal. There is nothing this builder declines that the reference CLI
/// would accept, so there is nothing to fall back to.
pub fn build(
    req: &BuildRequest,
    runtime_bin: &Path,
    raw_config: &Value,
) -> Result<String> {
    check_static(req.json)?;

    let requested = collect_features(raw_config, req.injected);
    let config_dir = req.config_path.parent().unwrap_or(req.worktree);
    let lock = super::lock::load(req.config_path);
    let mut resolved_lock = super::lock::Lockfile::default();
    let resolved = resolve_graph(requested, config_dir, &lock, &mut resolved_lock)?;
    // Written after resolution, before the build: the build is the slow part, and a lockfile
    // that records what was fetched is worth having even if installing it then fails.
    super::lock::save(req.config_path, &resolved_lock)?;

    // Ordering moves the Features around, so each one's content is looked up by id afterwards
    // rather than carried along positionally.
    let contents: BTreeMap<String, Content> = resolved
        .iter()
        .map(|(f, content)| (f.reference.raw.clone(), content.clone()))
        .collect();
    let override_order = override_install_order(raw_config);
    let features: Vec<feature::ResolvedFeature> = resolved.into_iter().map(|(f, _)| f).collect();
    // An entry naming nothing here does nothing at all, so the ordering the user wrote the key
    // for silently does not happen. Said once, before the slow part of the build.
    let unmatched = feature::unmatched_override_entries(&features, &override_order);
    if !unmatched.is_empty() {
        eprintln!(
            "{} overrideFeatureInstallOrder names {} which no Feature in this config \
             matches — the entry has no effect. Installed: {}",
            crate::color::warning_prefix(crate::color::enabled(crate::color::Stream::Stderr)),
            unmatched.join(", "),
            features
                .iter()
                .map(|f| f.reference.untagged().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    let ordered = feature::install_order(features, &override_order)?;

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
        let content = contents.get(&f.reference.raw).ok_or_else(|| {
            AmError::DevcontainerBuildFailed(format!("lost the content for {}", f.reference.raw))
        })?;
        cached_dirs.push(match content {
            Content::Layer(layer) => oci::fetch_layer(&f.reference, layer, &cache_root)?,
            Content::Directory(dir) => dir.clone(),
        });
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
    Ok(req.image.to_string())
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
        let (typed, _) = json(r#"{"image":"debian:bookworm-slim"}"#);
        assert!(check_static(&typed).is_ok());
    }

    #[test]
    fn a_registry_feature_config_is_supported() {
        let (typed, _) = json(
            r#"{"image":"debian","features":{"ghcr.io/devcontainers/features/git:1":{}}}"#,
        );
        assert!(check_static(&typed).is_ok());
    }

    #[test]
    fn a_compose_config_is_a_base_the_builder_accepts() {
        // The service's image can only be resolved by asking the runtime, so the static check
        // must not treat a compose config as baseless.
        let (typed, _) = json(r#"{"dockerComposeFile":"docker-compose.yml","service":"app"}"#);
        assert!(check_static(&typed).is_ok());
    }

    #[test]
    fn a_config_with_nothing_to_build_from_is_an_error_naming_what_to_add() {
        // Not a fallback: the reference CLI rejects this same config with "No image information
        // specified in devcontainer.json", so handing it over would only ask a second tool the
        // same unanswerable question — and, if the CLI is not installed, send the user to
        // install Node over a typo.
        let (typed, _) = json(r#"{"name":"nothing"}"#);
        let err = check_static(&typed).unwrap_err().to_string();
        assert!(err.contains("nothing to build from"), "got: {err}");
        for suggestion in ["image", "dockerfile", "dockerComposeFile"] {
            assert!(err.contains(suggestion), "must name {suggestion}: {err}");
        }
    }

    #[test]
    fn a_build_section_without_a_dockerfile_is_not_a_base() {
        // `build.dockerfile` has no default in the reference CLI either — a build section
        // without it is rejected the same way an empty config is.
        let (typed, _) = json(r#"{"build":{"context":"."}}"#);
        assert!(check_static(&typed).is_err());
    }

    #[test]
    fn override_install_order_is_supported() {
        let (typed, _) = json(
            r#"{"image":"debian","overrideFeatureInstallOrder":["ghcr.io/devcontainers/features/git"]}"#,
        );
        let (_, raw) = json(
            r#"{"image":"debian","overrideFeatureInstallOrder":["ghcr.io/devcontainers/features/git"]}"#,
        );
        assert!(check_static(&typed).is_ok());
        assert_eq!(override_install_order(&raw), vec!["ghcr.io/devcontainers/features/git"]);
    }

    #[test]
    fn a_config_without_an_override_list_yields_no_priorities() {
        let (_, raw) = json(r#"{"image":"debian"}"#);
        assert!(override_install_order(&raw).is_empty());
    }

    #[test]
    fn local_and_tarball_features_are_supported() {
        let (local, _) = json(r#"{"image":"debian","features":{"./local-feature":{}}}"#);
        assert!(check_static(&local).is_ok());
        let (tarball, _) = json(
            r#"{"image":"debian","features":{"https://example.com/devcontainer-feature-f.tgz":{}}}"#,
        );
        assert!(check_static(&tarball).is_ok());
    }

    #[test]
    fn every_kind_of_feature_reference_is_collected() {
        // `collect_features` used to silently drop everything that was not a registry ref,
        // because check_static had already rejected those configs. Now they must survive.
        let (_, raw) = json(
            r#"{"image":"debian","features":{
                "ghcr.io/devcontainers/features/git:1":{},
                "./vendored":{},
                "https://example.com/devcontainer-feature-f.tgz":{}
            }}"#,
        );
        let kinds: Vec<_> =
            collect_features(&raw, &[]).into_iter().map(|f| f.reference.kind).collect();
        assert_eq!(kinds.len(), 3);
        assert!(kinds.contains(&oci::FeatureKind::Registry));
        assert!(kinds.contains(&oci::FeatureKind::Local));
        assert!(kinds.contains(&oci::FeatureKind::Tarball));
    }

    /// Resolve with no lockfile involved, which is what the local-Feature tests want: the spec
    /// excludes local Features from the lockfile entirely.
    fn resolve_unlocked(
        requested: Vec<RequestedFeature>,
        config_dir: &Path,
    ) -> Result<Vec<(feature::ResolvedFeature, Content)>> {
        let empty = super::super::lock::Lockfile::default();
        let mut written = super::super::lock::Lockfile::default();
        resolve_graph(requested, config_dir, &empty, &mut written)
    }

    /// Write a local Feature into `dir`, optionally hard-depending on other local Features.
    fn write_local_feature(root: &Path, name: &str, depends_on: &[&str]) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let deps: String = depends_on
            .iter()
            .map(|d| format!("\"./{d}\":{{}}"))
            .collect::<Vec<_>>()
            .join(",");
        let depends = if deps.is_empty() {
            String::new()
        } else {
            format!(",\"dependsOn\":{{{deps}}}")
        };
        std::fs::write(
            dir.join("devcontainer-feature.json"),
            format!("{{\"id\":\"{name}\",\"version\":\"1.0.0\"{depends}}}"),
        )
        .unwrap();
        std::fs::write(dir.join("install.sh"), "#!/bin/sh\n").unwrap();
    }

    /// The whole resolver, offline.
    ///
    /// Local Features touch no registry, which makes them the only way to exercise
    /// `dependsOn`'s recursive fetch in the ordinary test suite — the registry path needs
    /// network, and no published Feature uses `dependsOn` to point at anyway.
    #[test]
    fn depends_on_pulls_in_features_the_config_never_named() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // top → middle → bottom, with only `top` named by the config.
        write_local_feature(dir, "top", &["middle"]);
        write_local_feature(dir, "middle", &["bottom"]);
        write_local_feature(dir, "bottom", &[]);

        let raw: Value = serde_json_lenient::from_str(
            r#"{"image":"debian","features":{"./top":{}}}"#,
        )
        .unwrap();
        let resolved = resolve_unlocked(collect_features(&raw, &[]), dir).unwrap();
        assert_eq!(resolved.len(), 3, "transitive dependencies must be pulled in");

        let ordered =
            feature::install_order(resolved.into_iter().map(|(f, _)| f).collect(), &[]).unwrap();
        assert_eq!(
            ordered.iter().map(|f| f.reference.raw.as_str()).collect::<Vec<_>>(),
            vec!["./bottom", "./middle", "./top"],
            "a hard dependency installs before whatever depends on it"
        );
    }

    #[test]
    fn a_diamond_installs_the_shared_dependency_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_local_feature(dir, "left", &["shared"]);
        write_local_feature(dir, "right", &["shared"]);
        write_local_feature(dir, "shared", &[]);

        let raw: Value = serde_json_lenient::from_str(
            r#"{"image":"debian","features":{"./left":{},"./right":{}}}"#,
        )
        .unwrap();
        let resolved = resolve_unlocked(collect_features(&raw, &[]), dir).unwrap();
        assert_eq!(resolved.len(), 3, "the shared dependency must not be installed twice");
    }

    #[test]
    fn a_depends_on_cycle_terminates_rather_than_looping() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        write_local_feature(dir, "a", &["b"]);
        write_local_feature(dir, "b", &["a"]);

        let raw: Value =
            serde_json_lenient::from_str(r#"{"image":"debian","features":{"./a":{}}}"#).unwrap();
        // Resolution itself must finish — identity dedup is what stops the walk.
        let resolved = resolve_unlocked(collect_features(&raw, &[]), dir).unwrap();
        assert_eq!(resolved.len(), 2);
        // The cycle is then reported by ordering, which is where the spec puts it.
        let err = feature::install_order(resolved.into_iter().map(|(f, _)| f).collect(), &[])
            .unwrap_err();
        assert!(err.to_string().contains("circularly"), "got: {err}");
    }

    #[test]
    fn a_local_feature_that_is_not_there_is_an_error_not_a_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let raw: Value =
            serde_json_lenient::from_str(r#"{"image":"debian","features":{"./missing":{}}}"#)
                .unwrap();
        let err = resolve_unlocked(collect_features(&raw, &[]), tmp.path()).unwrap_err();
        // The message has to name the path, since "no such file" alone would not say which.
        assert!(err.to_string().contains("./missing"), "got: {err}");
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
    fn two_tags_of_one_feature_are_two_features() {
        // Per the spec's equality rule these are distinct — different contents, so both
        // install. Deduplicating on the untagged name silently dropped the second.
        let (_, raw) = json(
            r#"{"image":"debian","features":{
                "ghcr.io/devcontainers/features/node:18":{},
                "ghcr.io/devcontainers/features/node:20":{}
            }}"#,
        );
        let collected = collect_features(&raw, &[]);
        assert_eq!(collected.len(), 2, "both tags were asked for");
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
    #[cfg_attr(
        not(feature = "integration-cli"),
        ignore = "needs --features integration-cli — the reference CLI, a runtime, and real image pulls"
    )]
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
    #[cfg_attr(
        not(feature = "integration-cli"),
        ignore = "needs --features integration-cli — the reference CLI, a runtime, and real image pulls"
    )]
    fn native_build_matches_the_cli_with_features_and_an_inherited_label() {
        assert_matches_reference(
            include_str!("../../../tests/fixtures/devcontainer/native/features-devcontainer.json"),
            include_str!("../../../tests/fixtures/devcontainer/native/cli-features-label.json"),
            "am-dc-native-difftest-features",
            None,
        );
    }

    /// Seven config-level metadata properties at once, which is what it takes to pin the order.
    ///
    /// The two fixtures above each exercise one or two keys, and agreed with the CLI by luck:
    /// the pairs they happen to contain sort the same way under either ordering. This one does
    /// not — `customizations` lands second here and last under the Feature order, so it fails
    /// loudly if the two lists are ever conflated again. It also covers `forwardPorts` and
    /// `portsAttributes` reaching the label at all, which they previously did not.
    #[test]
    #[cfg_attr(
        not(feature = "integration-cli"),
        ignore = "needs --features integration-cli — the reference CLI, a runtime, and real image pulls"
    )]
    fn native_build_matches_the_cli_across_the_config_metadata_schema() {
        assert_matches_reference(
            include_str!("../../../tests/fixtures/devcontainer/native/ports-devcontainer.json"),
            include_str!("../../../tests/fixtures/devcontainer/native/cli-ports-label.json"),
            "am-dc-native-difftest-ports",
            None,
        );
    }

    /// A Feature's `containerEnv` and lifecycle hooks — the two things a label comparison
    /// alone cannot check.
    ///
    /// `containerEnv` is deliberately absent from a Feature's label snippet, so the labels of a
    /// broken and a working build are *identical*; the difference is in the image's own
    /// environment. That is why this test asserts `Config.Env` as well, and why the bug went
    /// unnoticed: a Feature installed a toolchain and left it off `PATH`.
    #[test]
    #[cfg_attr(
        not(feature = "integration-cli"),
        ignore = "needs --features integration-cli — the reference CLI, a runtime, and real image pulls"
    )]
    fn native_build_matches_the_cli_for_feature_env_and_hooks() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/devcontainer/native");
        let image = "am-dc-native-difftest-env";
        assert_matches_reference_with(
            &std::fs::read_to_string(format!("{dir}/env-devcontainer.json")).unwrap(),
            &std::fs::read_to_string(format!("{dir}/cli-env-label.json")).unwrap(),
            image,
            None,
            // The Feature is copied in beside the config, exactly as a vendored one would be.
            &[],
        );

        let runtime = std::path::PathBuf::from(
            std::env::var("AM_DOCKER_BIN").unwrap_or_else(|_| "/usr/bin/docker".to_string()),
        );
        let ours = String::from_utf8(
            std::process::Command::new(&runtime)
                .args(["inspect", image, "--format", "{{ json .Config.Env }}"])
                .output()
                .expect("inspecting the built image")
                .stdout,
        )
        .unwrap();
        let ours: Vec<String> = serde_json::from_str(ours.trim()).unwrap();
        let theirs: Vec<String> = serde_json::from_str(
            std::fs::read_to_string(format!("{dir}/cli-env-image-env.json")).unwrap().trim(),
        )
        .unwrap();

        for expected in &theirs {
            assert!(
                ours.contains(expected),
                "the image is missing {expected:?} that the reference CLI bakes in\ngot: {ours:?}"
            );
        }
    }

    /// A compose config: the Features go onto the *service's* image, and the label that comes
    /// out has to be the same one the reference CLI produces for the same project.
    ///
    /// This is the build half of compose support. The run half — bringing the project up and
    /// exec'ing into the service — is covered by the cucumber scenarios, which can drive it
    /// against a mock runtime because nothing about it needs a real container.
    #[test]
    #[cfg_attr(
        not(feature = "integration-cli"),
        ignore = "needs --features integration-cli — the reference CLI, a runtime, and real image pulls"
    )]
    fn native_build_matches_the_cli_for_a_compose_service() {
        assert_matches_reference_with(
            include_str!("../../../tests/fixtures/devcontainer/native/compose-devcontainer.json"),
            include_str!("../../../tests/fixtures/devcontainer/native/cli-compose-label.json"),
            "am-dc-native-difftest-compose",
            None,
            &[(
                "docker-compose.yml",
                include_str!("../../../tests/fixtures/devcontainer/native/compose-project.yml"),
            )],
        );
    }

    /// Differential check of the *resolver* against the reference CLI.
    ///
    /// Much cheaper than the two above — `devcontainer features resolve-dependencies` walks the
    /// graph and prints the install order without building anything, so this costs a handful of
    /// manifest GETs rather than minutes of image build. It is also the sharper test of
    /// ordering: the label pins the order too, but only for the Features a build fixture
    /// happens to contain.
    ///
    /// The config is deliberately two *independent* `installsAfter` chains, which is what
    /// separates the spec's round-based algorithm from committing one eligible Feature at a
    /// time.
    ///
    /// The **digests** are compared too, not just the order. The CLI reports each Feature as
    /// `repo@sha256:…`, and that is the same manifest digest `am` keys identity on and writes
    /// into the lockfile — so this is what proves the two agree on what a Feature *is*, which
    /// no label comparison can show.
    #[test]
    #[cfg_attr(
        not(feature = "integration-cli"),
        ignore = "needs --features integration-cli — the reference CLI and real Feature pulls"
    )]
    fn install_order_matches_the_reference_cli_resolver() {
        assert_order_matches_reference(include_str!(
            "../../../tests/fixtures/devcontainer/native/two-chains-devcontainer.json"
        ));
    }

    /// The same four Features with an `overrideFeatureInstallOrder` that raises one of them.
    ///
    /// Worth its own case because the override does two separable things: it reorders Features
    /// inside a round, and it *splits* a round — deferring lower-priority Features that were
    /// otherwise ready. This fixture raises `common-utils`, which is the splitting case.
    #[test]
    #[cfg_attr(
        not(feature = "integration-cli"),
        ignore = "needs --features integration-cli — the reference CLI and real Feature pulls"
    )]
    fn override_install_order_matches_the_reference_cli_resolver() {
        assert_order_matches_reference(include_str!(
            "../../../tests/fixtures/devcontainer/native/override-order-devcontainer.json"
        ));
    }

    // ── Lockfile lifecycle, against the in-process registry ──────────────────
    //
    // These needed a published Feature and a reachable registry until the fixture existed. They
    // now run on every `cargo test`, which matters more than it sounds: the lockfile is what
    // decides *which code* gets installed and run as root during a build.

    use super::test_registry::{FakeRegistry, Feature as FixtureFeature};

    /// Resolving records the digest that was actually served, under the id the config wrote —
    /// which is the key a later build looks it up by.
    #[test]
    fn resolving_records_the_fetched_digest_in_the_lockfile() {
        let _cache = super::test_registry::CacheDir::new();
        let registry = FakeRegistry::with_feature(FixtureFeature::simple("amtest/base", "1.0.0", "base"));
        let id = registry.id("amtest/base", "1.0.0");
        let tmp = tempfile::tempdir().unwrap();

        let requested = vec![RequestedFeature {
            reference: oci::parse_ref(&id),
            options: BTreeMap::new(),
        }];
        let empty = super::super::lock::Lockfile::default();
        let mut written = super::super::lock::Lockfile::default();
        resolve_graph(requested, tmp.path(), &empty, &mut written).expect("resolve");

        let entry = written
            .features
            .get(&id)
            .unwrap_or_else(|| panic!("nothing recorded for {id}: {:?}", written.features));
        assert_eq!(
            entry.integrity,
            registry.feature("amtest/base").manifest_digest(),
            "the integrity recorded must be the manifest digest the registry served"
        );
        assert!(
            entry.resolved.ends_with(&format!("@{}", entry.integrity)),
            "resolved must qualify the id with that digest: {}",
            entry.resolved
        );
        // The counterpart to the locked test below: with nothing pinned, the tag is what gets
        // asked for. Without this, "the tag was not requested" over there could hold for a
        // build that made no request at all.
        assert!(
            registry.requested("/manifests/1.0.0"),
            "an unpinned build asks for the tag: {:?}",
            registry.requests()
        );
    }

    /// A build with a lockfile must ask the registry for the *pinned digest*, not the tag.
    ///
    /// This is the assertion the real-registry version could not make, and it is the whole
    /// point of a lockfile: without it a build can satisfy a lockfile it never consulted, and
    /// two people building the same config get different Features whenever the tag has moved.
    #[test]
    fn a_locked_build_requests_the_pinned_digest_rather_than_the_tag() {
        let _cache = super::test_registry::CacheDir::new();
        let feature = FixtureFeature::simple("amtest/base", "1.0.0", "base");
        let digest = feature.manifest_digest().to_string();
        let registry = FakeRegistry::with_feature(feature);
        let id = registry.id("amtest/base", "1.0.0");
        let tmp = tempfile::tempdir().unwrap();

        let mut lock = super::super::lock::Lockfile::default();
        lock.insert(
            &id,
            super::super::lock::LockEntry {
                version: Some("1.0.0".to_string()),
                resolved: format!("{}/amtest/base@{digest}", registry.host()),
                integrity: digest.clone(),
                depends_on: Vec::new(),
            },
        );

        let requested = vec![RequestedFeature {
            reference: oci::parse_ref(&id),
            options: BTreeMap::new(),
        }];
        let mut written = super::super::lock::Lockfile::default();
        resolve_graph(requested, tmp.path(), &lock, &mut written).expect("resolve");

        assert!(
            registry.requested(&format!("/manifests/{digest}")),
            "the pinned digest was never requested: {:?}",
            registry.requests()
        );
        assert!(
            !registry.requested("/manifests/1.0.0"),
            "the tag was requested despite a lockfile pinning a digest: {:?}",
            registry.requests()
        );
    }

    /// `dependsOn` resolved over the wire: a config naming one Feature installs two.
    ///
    /// There is already an offline version of this over local Features. This one is different
    /// in the part that matters — the dependency is declared in a *manifest annotation* fetched
    /// from a registry, which is the path every real config takes and the one the resolver's
    /// worklist actually walks.
    #[test]
    fn depends_on_over_a_registry_pulls_in_a_feature_the_config_never_named() {
        let _cache = super::test_registry::CacheDir::new();
        let registry = FakeRegistry::builder()
            .feature(FixtureFeature::simple("amtest/base", "1.0.0", "base"))
            // The dependency has to name the base by an id carrying this registry's port, which
            // only exists once the listener is bound.
            .start_with(|host| {
                vec![FixtureFeature::new(
                    "amtest/needs-base",
                    "1.0.0",
                    &format!(
                        r#"{{"id":"needs-base","version":"1.0.0","dependsOn":{{"{host}/amtest/base:1.0.0":{{}}}}}}"#
                    ),
                )]
            });
        let tmp = tempfile::tempdir().unwrap();

        let requested = vec![RequestedFeature {
            reference: oci::parse_ref(&registry.id("amtest/needs-base", "1.0.0")),
            options: BTreeMap::new(),
        }];
        let empty = super::super::lock::Lockfile::default();
        let mut written = super::super::lock::Lockfile::default();
        let resolved = resolve_graph(requested, tmp.path(), &empty, &mut written).expect("resolve");

        let ids: Vec<String> = resolved.iter().map(|(f, _)| f.reference.raw.clone()).collect();
        assert_eq!(ids.len(), 2, "the dependency should have been pulled in: {ids:?}");
        assert!(ids.iter().any(|i| i.contains("needs-base")), "{ids:?}");
        assert!(ids.iter().any(|i| i.ends_with("amtest/base:1.0.0")), "{ids:?}");
        // Both must be recorded, or a later locked build would re-resolve the dependency's tag.
        assert_eq!(written.features.len(), 2, "lockfile entries: {:?}", written.features);
    }

    /// The `dependsOn` differential test, which needs Features that actually declare one.
    ///
    /// Nothing in the common registries does, which is why this went unverified for so long.
    /// `scripts/test-registry.sh` publishes purpose-built Features to a local registry; this
    /// resolves a config using them through both implementations and compares.
    #[test]
    #[cfg_attr(
        not(all(feature = "integration-cli", feature = "integration-registry")),
        ignore = "needs --features integration-cli --features integration-registry — the reference CLI plus scripts/test-registry.sh"
    )]
    fn depends_on_matches_the_reference_cli_resolver() {
        assert_order_matches_reference(include_str!(
            "../../../tests/fixtures/devcontainer/native/depends-on-devcontainer.json"
        ));
    }

    /// Resolve `config_text` both ways and assert the install orders agree.
    fn assert_order_matches_reference(config_text: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("devcontainer.json"), config_text).unwrap();

        let cli = std::env::var("AM_DEVCONTAINER_BIN").unwrap_or_else(|_| "devcontainer".into());
        let out = std::process::Command::new(&cli)
            .args(["features", "resolve-dependencies", "--workspace-folder"])
            .arg(tmp.path())
            .output()
            .expect("running the reference CLI");
        assert!(out.status.success(), "CLI failed: {}", String::from_utf8_lossy(&out.stderr));

        // A mermaid flowchart precedes the JSON on stdout.
        let stdout = String::from_utf8_lossy(&out.stdout);
        let json_start = stdout.find('{').expect("no JSON in CLI output");
        let reported: Value = serde_json::from_str(&stdout[json_start..]).unwrap();
        // Kept whole: `repo@sha256:…`, so both the order and the resolved digest are compared.
        let expected: Vec<String> = reported["installOrder"]
            .as_array()
            .expect("installOrder")
            .iter()
            .map(|e| e["id"].as_str().unwrap().to_string())
            .collect();

        let raw: Value = serde_json_lenient::from_str(config_text).unwrap();
        let empty = super::super::lock::Lockfile::default();
        let mut written = super::super::lock::Lockfile::default();
        let resolved =
            resolve_graph(collect_features(&raw, &[]), &dir, &empty, &mut written).unwrap();
        let ordered = feature::install_order(
            resolved.into_iter().map(|(f, _)| f).collect(),
            &override_install_order(&raw),
        )
        .unwrap();

        // The lockfile `am` would write records `repo@digest` for each Feature — the same
        // string the CLI reports as a resolved id. Comparing them pins the format against the
        // reference implementation rather than against our own reading of the lockfile spec.
        let mut locked: Vec<String> =
            written.features.values().map(|e| e.resolved.clone()).collect();
        locked.sort();
        let mut reported_ids = expected.clone();
        reported_ids.sort();
        assert_eq!(locked, reported_ids, "lockfile entries diverged from the reference CLI");
        let actual: Vec<String> = ordered
            .iter()
            .map(|f| format!("{}@{}", f.reference.untagged(), f.digest))
            .collect();

        assert_eq!(
            actual, expected,
            "install order or resolved digest diverged from the reference CLI"
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
        assert_matches_reference_with(config_text, reference_label, image, probe, &[]);
    }

    /// As above, plus extra files to drop beside the `devcontainer.json` — a compose config is
    /// only half a config without the compose file it names.
    fn assert_matches_reference_with(
        config_text: &str,
        reference_label: &str,
        image: &str,
        probe: Option<(&str, &str, &str)>,
        extra_files: &[(&str, &str)],
    ) {
        let runtime = std::path::PathBuf::from(
            std::env::var("AM_DOCKER_BIN").unwrap_or_else(|_| "/usr/bin/docker".to_string()),
        );

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("devcontainer.json");
        std::fs::write(&config_path, config_text).unwrap();
        for (name, body) in extra_files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        // Any local Feature the fixtures carry, copied in beside the config so a `./name`
        // reference resolves the same way it would in a real repo.
        let fixtures = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/devcontainer/native"
        ));
        if let Ok(entries) = std::fs::read_dir(fixtures) {
            for entry in entries.filter_map(Result::ok).filter(|e| e.path().is_dir()) {
                let target = dir.join(entry.file_name());
                std::fs::create_dir_all(&target).unwrap();
                for file in std::fs::read_dir(entry.path()).unwrap().filter_map(Result::ok) {
                    std::fs::copy(file.path(), target.join(file.file_name())).unwrap();
                }
            }
        }

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
        .expect("build succeeds");
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
