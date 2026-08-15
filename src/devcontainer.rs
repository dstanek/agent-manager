//! Dev Container support: discovery, parsing, and configuration merge.
//!
//! `am` delegates image *building* to the reference `@devcontainers/cli` and keeps the
//! *run* path for itself (see `specs/devcontainer-support.md`). This module owns everything
//! needed to turn a repo's `.devcontainer/devcontainer.json` plus the built image's
//! `devcontainer.metadata` label into a `ResolvedConfig` that `container.rs` can run.
//!
//! Two sources feed the merge, and the split between them is not arbitrary — it is dictated
//! by what the CLI actually writes into the label:
//!
//! 1. The **label** carries feature contributions *and* the whole `devcontainer.json`
//!    reduced to the metadata schema. Elements are already in merge order (base image, then
//!    each Feature, then the config), so a left-to-right fold gets precedence right.
//! 2. **`devcontainer.json`** is parsed here only for the handful of properties the metadata
//!    schema drops: `runArgs`, `workspaceFolder`, `workspaceMount`, `initializeCommand`,
//!    `dockerComposeFile`, and `name`.
//!
//! Fixtures captured from a real CLI run live in `tests/fixtures/devcontainer/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::color;

use crate::error::AmError;

pub mod native;

// Path handling strategy: keep Path/PathBuf internally, convert at argv boundaries.
// See CLAUDE.md. Container-side paths are Strings because they are the *container's*
// namespace, not the host filesystem's, and never get opened locally.

// ── Lifecycle commands ────────────────────────────────────────────────────────

/// A lifecycle hook. The spec allows three shapes, and all three appear in the wild:
/// a shell string, an argv array (no shell), or a map of named commands that the
/// reference implementation runs in parallel.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum LifecycleCommand {
    Shell(String),
    Argv(Vec<String>),
    Named(BTreeMap<String, NamedCommand>),
}

/// The value side of a named lifecycle map. Same as `LifecycleCommand` minus nesting.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum NamedCommand {
    Shell(String),
    Argv(Vec<String>),
}

impl LifecycleCommand {
    /// Flatten into the individual commands to run, in a stable order.
    ///
    /// Named commands are sorted by key rather than left in map order: `am` runs them
    /// sequentially (not in parallel like the reference CLI), so a deterministic order is
    /// what makes a failure reproducible.
    pub fn commands(&self) -> Vec<Command> {
        match self {
            LifecycleCommand::Shell(s) => vec![Command::Shell(s.clone())],
            LifecycleCommand::Argv(a) => vec![Command::Argv(a.clone())],
            LifecycleCommand::Named(map) => map
                .values()
                .map(|v| match v {
                    NamedCommand::Shell(s) => Command::Shell(s.clone()),
                    NamedCommand::Argv(a) => Command::Argv(a.clone()),
                })
                .collect(),
        }
    }
}

/// A single runnable command, already flattened out of its named/array wrapper.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run through a shell — the string may contain pipes, `&&`, globs.
    Shell(String),
    /// Exec directly, no shell interpretation.
    Argv(Vec<String>),
}

// ── Mounts ────────────────────────────────────────────────────────────────────

/// A mount as it appears in the label. Features contribute objects; `devcontainer.json`
/// contributes strings; both land in the same array, hence the untagged enum.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Mount {
    Str(String),
    Obj(MountObject),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct MountObject {
    #[serde(default)]
    pub source: Option<String>,
    pub target: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub readonly: Option<bool>,
}

/// A mount reduced to the fields `am` acts on, regardless of which shape it arrived in.
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedMount {
    pub source: Option<String>,
    pub target: String,
    pub kind: String,
    pub read_only: bool,
}

impl Mount {
    /// Reduce either shape to a `NormalizedMount`.
    ///
    /// String mounts use docker's `--mount` key=value syntax. The aliases (`src`, `dst`,
    /// `destination`) are accepted because docker accepts them and configs in the wild use
    /// them interchangeably.
    pub fn normalize(&self) -> Result<NormalizedMount> {
        match self {
            Mount::Obj(o) => Ok(NormalizedMount {
                source: o.source.clone(),
                target: o.target.clone(),
                kind: o.kind.clone().unwrap_or_else(|| "bind".to_string()),
                read_only: o.readonly.unwrap_or(false),
            }),
            Mount::Str(s) => {
                let mut source = None;
                let mut target = None;
                let mut kind = None;
                let mut read_only = false;
                for part in s.split(',') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    let (key, value) = match part.split_once('=') {
                        Some((k, v)) => (k.trim(), v.trim()),
                        // Bare flags: `readonly` and `ro` are the documented spellings.
                        None => {
                            if part == "readonly" || part == "ro" {
                                read_only = true;
                            }
                            continue;
                        }
                    };
                    match key {
                        "source" | "src" => source = Some(value.to_string()),
                        "target" | "dst" | "destination" => target = Some(value.to_string()),
                        "type" => kind = Some(value.to_string()),
                        "readonly" | "ro" => read_only = value != "false",
                        _ => {} // consistency=cached and friends are advisory; ignore
                    }
                }
                let target = target.ok_or_else(|| {
                    AmError::ConfigError(format!(
                        "mount '{s}' has no target — expected 'target=' (or 'dst='/'destination=')"
                    ))
                })?;
                Ok(NormalizedMount {
                    source,
                    target,
                    kind: kind.unwrap_or_else(|| "bind".to_string()),
                    read_only,
                })
            }
        }
    }
}

// ── Image metadata label ──────────────────────────────────────────────────────

/// One element of the `devcontainer.metadata` image label.
///
/// Unknown properties (`customizations`, `hostRequirements`, …) are ignored rather than
/// rejected: the label is written by a tool that evolves faster than `am`, and an unknown
/// key must never be a hard error.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetadataSnippet {
    /// Feature id. Not consumed by the run path — kept because it is what identifies
    /// which Feature contributed a given entrypoint or mount when something looks wrong.
    #[allow(dead_code)]
    pub id: Option<String>,
    pub entrypoint: Option<String>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    pub init: Option<bool>,
    pub privileged: Option<bool>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub security_opt: Vec<String>,
    #[serde(default)]
    pub container_env: BTreeMap<String, String>,
    #[serde(default)]
    pub remote_env: BTreeMap<String, String>,
    pub container_user: Option<String>,
    pub remote_user: Option<String>,
    pub user_env_probe: Option<String>,
    pub override_command: Option<bool>,
    pub wait_for: Option<String>,
    pub on_create_command: Option<LifecycleCommand>,
    pub update_content_command: Option<LifecycleCommand>,
    pub post_create_command: Option<LifecycleCommand>,
    pub post_start_command: Option<LifecycleCommand>,
    pub post_attach_command: Option<LifecycleCommand>,
    /// Ports the config asked to be reachable. Kept as raw values because the entries are a
    /// union of numbers and `"<service>:<port>"` strings; [`ForwardedPort::parse`] sorts them out.
    #[serde(default)]
    pub forward_ports: Vec<serde_json::Value>,
}

/// How to derive the environment the agent runs with.
///
/// A devcontainer's toolchain is frequently installed by something that appends to `PATH` in a
/// dotfile — nvm, rbenv, sdkman, a Feature's own `.bashrc` line. A process started directly in
/// the container never sources those, so the agent would not see tools that are plainly there in
/// an editor terminal. This is the spec's answer: run the user's shell, capture the environment
/// it ends up with, and apply it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserEnvProbe {
    /// Start the agent with exactly the environment `am` and the image provide.
    #[default]
    None,
    InteractiveShell,
    LoginShell,
    LoginInteractiveShell,
}

impl UserEnvProbe {
    /// Parse the config value. The spec's default when the key is absent is
    /// `loginInteractiveShell`, which is what an unrecognised value falls back to as well.
    pub fn parse(value: Option<&str>) -> Self {
        match value {
            Some("none") => UserEnvProbe::None,
            Some("interactiveShell") => UserEnvProbe::InteractiveShell,
            Some("loginShell") => UserEnvProbe::LoginShell,
            _ => UserEnvProbe::LoginInteractiveShell,
        }
    }

    /// The flags the probe shell is invoked with, or `None` for no probe at all.
    ///
    /// Taken from the reference CLI, which runs `<shell> -lic 'cat /proc/self/environ'`.
    pub fn shell_flags(&self) -> Option<&'static str> {
        match self {
            UserEnvProbe::None => None,
            UserEnvProbe::InteractiveShell => Some("-ic"),
            UserEnvProbe::LoginShell => Some("-lc"),
            UserEnvProbe::LoginInteractiveShell => Some("-lic"),
        }
    }
}

/// A port a devcontainer asked to be reachable from the machine running `am`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardedPort {
    /// A port on the container the agent runs in.
    Own(u16),
    /// `"<service>:<port>"` — a port on another compose service. Meaningless outside a compose
    /// project, where there is only one container to publish from.
    Service { service: String, port: u16 },
}

impl ForwardedPort {
    /// Read one `forwardPorts` entry. Anything unparseable yields `None` rather than an error:
    /// the property is a convenience, and refusing to start a session over a malformed port
    /// would be a worse outcome than not publishing it.
    pub fn parse(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::Number(n) => n.as_u64()?.try_into().ok().map(ForwardedPort::Own),
            serde_json::Value::String(s) => match s.rsplit_once(':') {
                Some((service, port)) if !service.is_empty() => Some(ForwardedPort::Service {
                    service: service.to_string(),
                    port: port.parse().ok()?,
                }),
                _ => s.parse().ok().map(ForwardedPort::Own),
            },
            _ => None,
        }
    }

    /// The compose/`-p` publish spec for this port.
    ///
    /// Bound to loopback, which is both what the reference CLI does for a bare `appPort` and the
    /// conservative reading of "forward this to me": a session container is not something to put
    /// on the network by default.
    pub fn publish_spec(port: u16) -> String {
        format!("127.0.0.1:{port}:{port}")
    }
}

/// The label is an array of snippets, but the schema also permits a bare object.
/// 0.88 emitted an array in every observed case; accept both anyway.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum MetadataLabel {
    Many(Vec<MetadataSnippet>),
    One(Box<MetadataSnippet>),
}

/// Parse the `devcontainer.metadata` label value into snippets in merge order.
pub fn parse_metadata_label(text: &str) -> Result<Vec<MetadataSnippet>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let label: MetadataLabel = serde_json_lenient::from_str(trimmed)
        .with_context(|| "parsing the devcontainer.metadata image label")?;
    Ok(match label {
        MetadataLabel::Many(v) => v,
        MetadataLabel::One(s) => vec![*s],
    })
}

// ── devcontainer.json (the properties the label drops) ────────────────────────

/// `dockerComposeFile` is a string or an array of strings. `am` only needs to know it is
/// present in order to reject the config, so the contents are kept opaque.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ComposeFile {
    One(String),
    Many(Vec<String>),
}

/// The subset of `devcontainer.json` that does **not** survive into the image label.
///
/// Everything else deliberately comes from the label instead, so this struct stays small.
/// `image` and `build` are what the native builder needs to resolve a base image; in CLI mode
/// `build` is used only to locate the Dockerfile for hashing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DevcontainerJson {
    pub name: Option<String>,
    #[serde(default)]
    pub run_args: Vec<String>,
    pub workspace_folder: Option<String>,
    pub workspace_mount: Option<String>,
    pub initialize_command: Option<LifecycleCommand>,
    pub docker_compose_file: Option<ComposeFile>,
    /// The compose service the agent runs in. Required alongside `dockerComposeFile`.
    pub service: Option<String>,
    pub image: Option<String>,
    pub build: Option<BuildSection>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildSection {
    pub dockerfile: Option<String>,
    /// Build context, relative to the config file. Defaults to the Dockerfile's directory.
    pub context: Option<String>,
    #[serde(default)]
    pub args: BTreeMap<String, serde_json::Value>,
    pub target: Option<String>,
}

/// Parse a `devcontainer.json`. The file is JSONC — comments and trailing commas are
/// normal in configs written for editors, and the reference CLI accepts them.
pub fn parse_config(path: &Path) -> Result<DevcontainerJson> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading devcontainer config {}", path.display()))?;
    parse_config_str(&text)
        .with_context(|| format!("parsing devcontainer config {}", path.display()))
}

fn parse_config_str(text: &str) -> Result<DevcontainerJson> {
    serde_json_lenient::from_str(text).map_err(|e| AmError::ConfigError(e.to_string()).into())
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Locate a devcontainer config inside a session worktree.
///
/// Resolution is relative to the *worktree*, not the repo root: the config is a checked-in,
/// branch-specific file, so two sessions on different branches can legitimately disagree
/// about it.
///
/// Order: explicit override → `.devcontainer/devcontainer.json` → `.devcontainer.json` →
/// `.devcontainer/<folder>/devcontainer.json`. The last form is only accepted when exactly
/// one match exists; several is ambiguous and `am` will not guess.
pub fn find_config(worktree: &Path, override_path: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Some(rel) = override_path {
        let path = if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            worktree.join(rel)
        };
        if !path.is_file() {
            return Err(AmError::ConfigError(format!(
                "devcontainer config not found at {} (from devcontainer.path)",
                path.display()
            ))
            .into());
        }
        return Ok(Some(path));
    }

    let primary = worktree.join(".devcontainer").join("devcontainer.json");
    if primary.is_file() {
        return Ok(Some(primary));
    }

    let dotfile = worktree.join(".devcontainer.json");
    if dotfile.is_file() {
        return Ok(Some(dotfile));
    }

    let dir = worktree.join(".devcontainer");
    if !dir.is_dir() {
        return Ok(None);
    }
    let mut matches: Vec<PathBuf> = Vec::new();
    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("scanning {} for devcontainer configs", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        if entry.path().is_dir() {
            let candidate = entry.path().join("devcontainer.json");
            if candidate.is_file() {
                matches.push(candidate);
            }
        }
    }
    matches.sort();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(Some(matches.remove(0))),
        _ => {
            let list = matches
                .iter()
                .map(|p| format!("  {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n");
            Err(AmError::ConfigError(format!(
                "several devcontainer configs found under {}:\n{list}\n\
                 Set devcontainer.path in .am/config.toml to choose one",
                dir.display()
            ))
            .into())
        }
    }
}

// ── Variable substitution ─────────────────────────────────────────────────────

/// Values available to `${...}` substitution.
///
/// The CLI does *not* substitute before writing the label — `${localWorkspaceFolder}`
/// survives verbatim — so this runs over label-sourced properties too, not just over
/// `devcontainer.json`.
#[derive(Debug, Clone)]
pub struct SubstitutionContext {
    pub local_workspace_folder: PathBuf,
    pub container_workspace_folder: String,
    pub container_env: BTreeMap<String, String>,
}

impl SubstitutionContext {
    pub fn new(local_workspace_folder: &Path, container_workspace_folder: &str) -> Self {
        Self {
            local_workspace_folder: local_workspace_folder.to_path_buf(),
            container_workspace_folder: container_workspace_folder.to_string(),
            container_env: BTreeMap::new(),
        }
    }

    fn basename(path: &Path) -> String {
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Resolve a single `${...}` body. Unknown variables resolve to an empty string, which
    /// is what the reference implementation does — erroring would reject configs that work
    /// fine in an editor.
    fn resolve(&self, name: &str) -> String {
        if let Some(var) = name.strip_prefix("localEnv:") {
            return std::env::var(var).unwrap_or_default();
        }
        if let Some(var) = name.strip_prefix("containerEnv:") {
            return self.container_env.get(var).cloned().unwrap_or_default();
        }
        match name {
            "localWorkspaceFolder" => self.local_workspace_folder.to_string_lossy().into_owned(),
            "containerWorkspaceFolder" => self.container_workspace_folder.clone(),
            "localWorkspaceFolderBasename" => Self::basename(&self.local_workspace_folder),
            "containerWorkspaceFolderBasename" => {
                Self::basename(Path::new(&self.container_workspace_folder))
            }
            _ => String::new(),
        }
    }

    /// Expand every `${...}` occurrence in `input`.
    ///
    /// An unterminated `${` is emitted literally rather than treated as an error — it is
    /// far more likely to be a shell snippet inside a lifecycle command than a typo'd
    /// variable, and eating it would silently corrupt the command.
    pub fn substitute(&self, input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            match after.find('}') {
                Some(end) => {
                    out.push_str(&self.resolve(&after[..end]));
                    rest = &after[end + 1..];
                }
                None => {
                    out.push_str(&rest[start..]);
                    return out;
                }
            }
        }
        out.push_str(rest);
        out
    }
}

// ── Merge ─────────────────────────────────────────────────────────────────────

/// The merged, substituted configuration the run path consumes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedConfig {
    /// Feature entrypoints, in contribution order. Composed ahead of the agent command.
    pub entrypoints: Vec<String>,
    pub mounts: Vec<NormalizedMount>,
    pub init: bool,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub container_env: BTreeMap<String, String>,
    pub remote_env: BTreeMap<String, String>,
    pub container_user: Option<String>,
    pub remote_user: Option<String>,
    pub user_env_probe: Option<String>,
    pub override_command: Option<bool>,
    pub wait_for: Option<String>,
    pub on_create: Vec<Command>,
    pub update_content: Vec<Command>,
    pub post_create: Vec<Command>,
    pub post_start: Vec<Command>,
    pub post_attach: Vec<Command>,
    /// Ports to publish, in contribution order and de-duplicated.
    pub forward_ports: Vec<ForwardedPort>,
    /// From `devcontainer.json` only — the label drops these.
    pub run_args: Vec<String>,
    pub workspace_folder: Option<String>,
    pub workspace_mount: Option<String>,
    pub name: Option<String>,
}

/// Append `values` to `target`, skipping duplicates. Used for the union-merged lists.
fn extend_unique(target: &mut Vec<String>, values: &[String]) {
    for v in values {
        if !target.iter().any(|existing| existing == v) {
            target.push(v.clone());
        }
    }
}

/// Merge label snippets left-to-right into a `ResolvedConfig`.
///
/// Precedence is positional: the CLI emits base-image metadata first, then Features, then
/// `devcontainer.json`, so later elements win for scalar properties. The list-valued
/// properties follow the spec's own rules rather than last-wins.
pub fn merge(snippets: &[MetadataSnippet]) -> Result<ResolvedConfig> {
    let mut out = ResolvedConfig::default();
    for snippet in snippets {
        if let Some(ref ep) = snippet.entrypoint {
            out.entrypoints.push(ep.clone());
        }
        for mount in &snippet.mounts {
            let normalized = mount.normalize()?;
            // Last writer for a given target wins, but keeps the earlier position so that
            // mount order stays stable across rebuilds.
            match out.mounts.iter_mut().find(|m| m.target == normalized.target) {
                Some(existing) => *existing = normalized,
                None => out.mounts.push(normalized),
            }
        }
        out.init |= snippet.init.unwrap_or(false);
        out.privileged |= snippet.privileged.unwrap_or(false);
        extend_unique(&mut out.cap_add, &snippet.cap_add);
        extend_unique(&mut out.security_opt, &snippet.security_opt);
        for (k, v) in &snippet.container_env {
            out.container_env.insert(k.clone(), v.clone());
        }
        for (k, v) in &snippet.remote_env {
            out.remote_env.insert(k.clone(), v.clone());
        }
        if snippet.container_user.is_some() {
            out.container_user = snippet.container_user.clone();
        }
        if snippet.remote_user.is_some() {
            out.remote_user = snippet.remote_user.clone();
        }
        if snippet.user_env_probe.is_some() {
            out.user_env_probe = snippet.user_env_probe.clone();
        }
        if snippet.override_command.is_some() {
            out.override_command = snippet.override_command;
        }
        if snippet.wait_for.is_some() {
            out.wait_for = snippet.wait_for.clone();
        }
        collect(&mut out.on_create, &snippet.on_create_command);
        collect(&mut out.update_content, &snippet.update_content_command);
        collect(&mut out.post_create, &snippet.post_create_command);
        collect(&mut out.post_start, &snippet.post_start_command);
        collect(&mut out.post_attach, &snippet.post_attach_command);
        // A union rather than last-writer-wins: forwardPorts is a list of things that should be
        // reachable, so a later snippet asking for one more must not drop the earlier ones.
        for value in &snippet.forward_ports {
            if let Some(port) = ForwardedPort::parse(value) {
                if !out.forward_ports.contains(&port) {
                    out.forward_ports.push(port);
                }
            }
        }
    }
    Ok(out)
}

fn collect(target: &mut Vec<Command>, cmd: &Option<LifecycleCommand>) {
    if let Some(c) = cmd {
        target.extend(c.commands());
    }
}

/// Fold in the `devcontainer.json`-only properties, then substitute variables everywhere.
///
/// Substitution happens last so that `${containerEnv:VAR}` can see the fully merged
/// environment rather than whichever fragment happened to define it first.
pub fn finalize(
    mut resolved: ResolvedConfig,
    json: &DevcontainerJson,
    ctx: &SubstitutionContext,
) -> ResolvedConfig {
    resolved.run_args = json.run_args.clone();
    resolved.workspace_folder = json.workspace_folder.clone();
    resolved.workspace_mount = json.workspace_mount.clone();
    resolved.name = json.name.clone();

    let mut ctx = ctx.clone();
    ctx.container_env = resolved.container_env.clone();

    resolved.container_env = resolved
        .container_env
        .iter()
        .map(|(k, v)| (k.clone(), ctx.substitute(v)))
        .collect();
    resolved.remote_env = resolved
        .remote_env
        .iter()
        .map(|(k, v)| (k.clone(), ctx.substitute(v)))
        .collect();
    for mount in &mut resolved.mounts {
        mount.source = mount.source.as_deref().map(|s| ctx.substitute(s));
        mount.target = ctx.substitute(&mount.target);
    }
    for arg in &mut resolved.run_args {
        *arg = ctx.substitute(arg);
    }
    resolved.workspace_folder = resolved.workspace_folder.as_deref().map(|s| ctx.substitute(s));
    resolved.workspace_mount = resolved.workspace_mount.as_deref().map(|s| ctx.substitute(s));
    for list in [
        &mut resolved.on_create,
        &mut resolved.update_content,
        &mut resolved.post_create,
        &mut resolved.post_start,
        &mut resolved.post_attach,
    ] {
        for cmd in list.iter_mut() {
            *cmd = match cmd {
                Command::Shell(s) => Command::Shell(ctx.substitute(s)),
                Command::Argv(a) => Command::Argv(a.iter().map(|s| ctx.substitute(s)).collect()),
            };
        }
    }
    resolved
}

// ── Image identity ────────────────────────────────────────────────────────────

/// A Feature `am` injects at build time that the project's own config knows nothing
/// about — the agent's Feature, plus anything in `devcontainer.extra_features`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InjectedFeature {
    pub id: String,
    /// Feature options as raw JSON. `"{}"` for defaults.
    pub options: String,
}

impl InjectedFeature {
    pub fn new(id: &str, options: &str) -> Self {
        Self {
            id: id.to_string(),
            options: options.to_string(),
        }
    }

    pub fn with_defaults(id: &str) -> Self {
        Self::new(id, "{}")
    }
}

/// Compute the image name for a config: `am-dc-<hash>`.
///
/// The hash covers the config bytes, the referenced Dockerfile if any, and the injected
/// features, so an unchanged config reuses its image and never invokes the Node CLI.
///
/// **Known limitation:** other files in the build context are not hashed. Hashing an
/// arbitrary context is unbounded work (`"context": ".."` means the whole repo), so editing
/// a file that the Dockerfile `COPY`s will not by itself trigger a rebuild — use
/// `am start --rebuild`.
pub fn image_name(config_path: &Path, injected: &[InjectedFeature]) -> Result<String> {
    Ok(format!("am-dc-{}", config_hash(config_path, injected)?))
}

pub fn config_hash(config_path: &Path, injected: &[InjectedFeature]) -> Result<String> {
    use sha2::{Digest, Sha256};

    let bytes = std::fs::read(config_path)
        .with_context(|| format!("reading devcontainer config {}", config_path.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);

    let json = parse_config_str(&String::from_utf8_lossy(&bytes))?;
    if let Some(dockerfile) = dockerfile_path(config_path, &json) {
        // A missing Dockerfile is not this function's error to raise — the build step will
        // report it far more clearly. Hash the path so the name still differs.
        match std::fs::read(&dockerfile) {
            Ok(content) => hasher.update(&content),
            Err(_) => hasher.update(dockerfile.to_string_lossy().as_bytes()),
        }
    }

    // Sorted so that config ordering, which carries no meaning, cannot change the name.
    let mut features = injected.to_vec();
    features.sort();
    for f in features {
        hasher.update(f.id.as_bytes());
        hasher.update(f.options.as_bytes());
    }

    let digest = hasher.finalize();
    Ok(digest.iter().take(6).map(|b| format!("{b:02x}")).collect())
}

// ── Build step ────────────────────────────────────────────────────────────────

/// Everything a builder needs to turn a config into an image.
///
/// Both builders take the same request and return the same thing — an image name — which is
/// what keeps the run path from having to know which one produced it.
pub struct BuildRequest<'a> {
    pub worktree: &'a Path,
    pub config_path: &'a Path,
    pub json: &'a DevcontainerJson,
    pub image: &'a str,
    pub injected: &'a [InjectedFeature],
    pub no_cache: bool,
}

/// Build the image with whichever builder the config selects.
///
/// In [`crate::config::Builder::Auto`] an unsupported construct is a fallback, not an error:
/// `am` says what it could not handle and hands off to the reference CLI. In
/// [`crate::config::Builder::Native`] the same condition is fatal, so that a config which
/// silently costs a Node dependency is visible rather than invisible.
pub fn build_image(
    req: &BuildRequest,
    cfg: &crate::config::Config,
    runtime_bin: &Path,
) -> Result<String> {
    use crate::config::Builder;

    if cfg.devcontainer.builder == Builder::Cli {
        return build_with_cli(req, cfg, runtime_bin);
    }

    // The native builder needs the config as raw JSON: the properties it copies into the
    // metadata label are deliberately not modelled, so they can pass through untouched.
    let text = std::fs::read_to_string(req.config_path)
        .with_context(|| format!("reading {}", req.config_path.display()))?;
    let raw: serde_json::Value = serde_json_lenient::from_str(&text)
        .map_err(|e| AmError::ConfigError(e.to_string()))?;

    match native::build(req, runtime_bin, &raw)? {
        Ok(image) => Ok(image),
        Err(reason) if cfg.devcontainer.builder == Builder::Native => {
            Err(AmError::DevcontainerBuildFailed(format!(
                "am's own builder cannot handle this config: {reason}\n\
                 Set devcontainer.builder = \"auto\" to fall back to the reference CLI"
            ))
            .into())
        }
        Err(reason) => {
            println!("Falling back to the devcontainer CLI: {reason}.");
            build_with_cli(req, cfg, runtime_bin)
        }
    }
}

/// Delegate to `@devcontainers/cli`.
fn build_with_cli(
    req: &BuildRequest,
    cfg: &crate::config::Config,
    runtime_bin: &Path,
) -> Result<String> {
    let cli = find_cli(&cfg.devcontainer.cli)?;
    build(
        &cli,
        req.worktree,
        req.config_path,
        req.image,
        req.injected,
        runtime_bin,
        req.no_cache,
    )
}

/// Locate the `devcontainer` CLI.
///
/// `AM_DEVCONTAINER_BIN` takes precedence and is used *exclusively* when set, matching
/// how `container.rs` resolves runtimes — that is what lets tests inject a mock, or a
/// deliberately nonexistent path to simulate "not installed".
pub fn find_cli(configured: &str) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("AM_DEVCONTAINER_BIN") {
        let p = PathBuf::from(&path);
        return if p.exists() {
            Ok(p)
        } else {
            Err(AmError::DevcontainerCliNotFound(path).into())
        };
    }
    which::which(configured)
        .map_err(|_| AmError::DevcontainerCliNotFound(configured.to_string()).into())
}

/// Assemble the `devcontainer build` argv.
///
/// Subcommand options must come **after** the subcommand — putting `--docker-path` before
/// `build` makes the CLI ignore it and silently shell out to `docker`, which is a confusing
/// failure on a podman-only machine.
pub fn build_command_args(
    worktree: &Path,
    config_path: &Path,
    image: &str,
    injected: &[InjectedFeature],
    docker_path: &Path,
    no_cache: bool,
) -> Vec<String> {
    let mut args = vec![
        "build".to_string(),
        "--workspace-folder".to_string(),
        worktree.to_string_lossy().into_owned(),
        "--config".to_string(),
        config_path.to_string_lossy().into_owned(),
        "--image-name".to_string(),
        image.to_string(),
        "--docker-path".to_string(),
        docker_path.to_string_lossy().into_owned(),
    ];
    if no_cache {
        args.push("--no-cache".to_string());
    }
    if !injected.is_empty() {
        args.push("--additional-features".to_string());
        args.push(additional_features_json(injected));
    }
    args
}

/// Render injected features as the JSON object `--additional-features` expects.
fn additional_features_json(injected: &[InjectedFeature]) -> String {
    let mut map = serde_json::Map::new();
    for feature in injected {
        // Options come from am's own config, but a malformed value should degrade to
        // "defaults" rather than abort a build over a cosmetic mistake.
        let value = serde_json_lenient::from_str(&feature.options)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
        map.insert(feature.id.clone(), value);
    }
    serde_json::Value::Object(map).to_string()
}

/// The JSON `devcontainer build` prints on stdout.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildResult {
    outcome: String,
    #[serde(default)]
    image_name: Option<ReportedImageName>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// 0.88 reports an array; the schema also permits a bare string.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ReportedImageName {
    One(String),
    Many(Vec<String>),
}

impl ReportedImageName {
    fn first(self) -> Option<String> {
        match self {
            ReportedImageName::One(s) => Some(s),
            ReportedImageName::Many(v) => v.into_iter().next(),
        }
    }
}

/// Run `devcontainer build` and return the built image name.
///
/// Build progress goes to stderr and is passed through to the user's terminal — a feature
/// install can take minutes and silence would look like a hang. Only stdout is captured,
/// which is where the result JSON lands.
pub fn build(
    cli: &Path,
    worktree: &Path,
    config_path: &Path,
    image: &str,
    injected: &[InjectedFeature],
    docker_path: &Path,
    no_cache: bool,
) -> Result<String> {
    let args = build_command_args(worktree, config_path, image, injected, docker_path, no_cache);
    let output = std::process::Command::new(cli)
        .args(&args)
        .stderr(std::process::Stdio::inherit())
        .output()
        .map_err(|e| {
            AmError::DevcontainerBuildFailed(format!("failed to run {}: {e}", cli.display()))
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Both signals are checked: the CLI exits 1 on failure *and* reports outcome in the
    // JSON, and neither alone is worth trusting across CLI versions.
    let parsed: Option<BuildResult> = stdout
        .lines()
        .rev()
        .find_map(|line| serde_json_lenient::from_str(line.trim()).ok());

    match parsed {
        Some(result) if result.outcome == "success" && output.status.success() => {
            // Prefer the name the CLI reports over the one we asked for, so a future
            // release that normalises tags does not leave `am` running a stale image.
            Ok(result
                .image_name
                .and_then(ReportedImageName::first)
                .unwrap_or_else(|| image.to_string()))
        }
        Some(result) => {
            let detail = [result.description, result.message]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": ");
            Err(AmError::DevcontainerBuildFailed(if detail.is_empty() {
                format!("outcome '{}'", result.outcome)
            } else {
                detail
            })
            .into())
        }
        None if output.status.success() => Err(AmError::DevcontainerBuildFailed(
            "the CLI reported success but printed no result JSON — is `devcontainer` the \
             reference CLI from @devcontainers/cli?"
                .to_string(),
        )
        .into()),
        None => Err(AmError::DevcontainerBuildFailed(format!(
            "the CLI exited with {} (see the output above)",
            output.status
        ))
        .into()),
    }
}

// ── Reading the built image ───────────────────────────────────────────────────

/// Whether an image is already present locally. A present image means the build is
/// skipped entirely, which is what keeps Node off the per-session path.
pub fn image_exists(runtime_bin: &Path, image: &str) -> bool {
    std::process::Command::new(runtime_bin)
        .args(["image", "inspect", image])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Read and parse the `devcontainer.metadata` label from a built image.
pub fn read_image_metadata(runtime_bin: &Path, image: &str) -> Result<Vec<MetadataSnippet>> {
    let output = std::process::Command::new(runtime_bin)
        .args([
            "inspect",
            image,
            "--format",
            "{{ index .Config.Labels \"devcontainer.metadata\" }}",
        ])
        .output()
        .map_err(|e| AmError::ContainerError(format!("failed to inspect image {image}: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AmError::ContainerError(format!(
            "could not inspect image {image}: {stderr}"
        ))
        .into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // An image built from a plain `"image": "..."` config with no Features carries no
    // label at all; podman prints `<no value>` for the missing key. Not an error.
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "<no value>" {
        return Ok(Vec::new());
    }
    parse_metadata_label(trimmed)
}

/// Resolve `build.dockerfile` relative to the config file's directory.
pub fn dockerfile_path(config_path: &Path, json: &DevcontainerJson) -> Option<PathBuf> {
    let dockerfile = json.build.as_ref()?.dockerfile.as_deref()?;
    let base = config_path.parent()?;
    Some(base.join(dockerfile))
}

/// Reject configs that use constructs `am` does not implement yet, with a message that
/// says what to do instead rather than just naming the unsupported key.
///
/// Compose used to be rejected here. It is now supported (see [`crate::compose`]), but it does
/// require `service` — without it there is no way to know which container the agent belongs in,
/// and guessing would put it somewhere arbitrary.
pub fn check_supported(json: &DevcontainerJson) -> Result<()> {
    if json.docker_compose_file.is_some() && json.service.is_none() {
        return Err(AmError::ConfigError(
            "this devcontainer sets dockerComposeFile but no service, so am cannot tell which \
             container the agent belongs in\n\
             Add \"service\": \"<name>\" to the devcontainer.json"
                .to_string(),
        )
        .into());
    }
    Ok(())
}

/// The compose files a config names, resolved against the directory holding it.
///
/// Order matters and is preserved: compose layers later files over earlier ones, and `am` adds
/// its own override after all of them.
pub fn compose_files(config_path: &Path, json: &DevcontainerJson) -> Vec<PathBuf> {
    let base = config_path.parent().unwrap_or(Path::new("."));
    match &json.docker_compose_file {
        Some(ComposeFile::One(one)) => vec![base.join(one)],
        Some(ComposeFile::Many(many)) => many.iter().map(|f| base.join(f)).collect(),
        None => Vec::new(),
    }
}

// ── Lifecycle hooks ───────────────────────────────────────────────────────────

/// The hooks that must run inside the container before the agent starts, flattened into
/// shell snippets in spec order, paired with the hook names for the session record.
///
/// **Semantics note.** The spec distinguishes create-time hooks (`onCreateCommand`,
/// `updateContentCommand`, `postCreateCommand` — once per container) from start-time
/// (`postStartCommand` — every start). `am` runs containers with `--rm`, so every
/// `am start` creates a *new* container and the previous one's filesystem is gone: a
/// create-time hook that installed dependencies must run again or the environment is
/// broken. Running them each time is therefore the correct behaviour here, not a
/// shortcut. `lifecycle_done` records what ran so a future persistent-container mode can
/// skip them.
///
/// `postAttachCommand` is not run: `am attach` moves tmux focus, it does not attach to
/// the container, so there is no attach event to hang it off.
pub fn startup_commands(resolved: &ResolvedConfig, skip: bool) -> (Vec<String>, Vec<String>) {
    if skip {
        return (Vec::new(), Vec::new());
    }
    let stages: [(&str, &Vec<Command>); 4] = [
        ("onCreateCommand", &resolved.on_create),
        ("updateContentCommand", &resolved.update_content),
        ("postCreateCommand", &resolved.post_create),
        ("postStartCommand", &resolved.post_start),
    ];
    let mut snippets = Vec::new();
    let mut ran = Vec::new();
    for (name, commands) in stages {
        if commands.is_empty() {
            continue;
        }
        ran.push(name.to_string());
        for command in commands {
            snippets.push(command.to_shell());
        }
    }
    (snippets, ran)
}

impl Command {
    /// Render as a snippet suitable for `sh -c`.
    pub fn to_shell(&self) -> String {
        match self {
            // Already a shell string: the spec says to run it through a shell, so pipes
            // and && inside it are intentional and must not be quoted away.
            Command::Shell(s) => s.clone(),
            Command::Argv(args) => args
                .iter()
                .map(|a| crate::command::shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

// ── Trust ─────────────────────────────────────────────────────────────────────

/// Refuse to run `initializeCommand` unless explicitly allowed.
///
/// This hook runs on the **host**, outside every isolation boundary `am` exists to
/// provide, and `devcontainer.json` is repo-controlled code that arrives with a `git pull`.
/// Owning the run path means `am` simply never executes it — there is no flag to get wrong
/// and no CLI to trust. The delegated `devcontainer build` does not run it either.
pub fn check_host_commands(json: &DevcontainerJson, allowed: bool) -> Result<()> {
    if json.initialize_command.is_some() && !allowed {
        return Err(AmError::ConfigError(
            "this devcontainer defines initializeCommand, which runs on your host rather \
             than inside the container\n\
             am does not run it by default. Set devcontainer.allow_host_commands = true in \
             .am/config.toml if you have read the command and trust it."
                .to_string(),
        )
        .into());
    }
    Ok(())
}

/// Translate a resolved config into runtime settings, dropping options `am` will not grant.
///
/// Escalating options come from a file in the repo, so they are opt-in rather than
/// automatic. Dropping them is deliberately non-fatal: most containers still work without
/// `privileged`, and failing outright would make an ordinary config unusable over a
/// capability it may not even need.
pub fn apply_trust(
    resolved: &ResolvedConfig,
    cfg: &crate::config::Config,
) -> crate::container::DevcontainerRuntime {
    let allow = cfg.devcontainer.allow_host_commands;
    let mut env: Vec<(String, String)> = resolved
        .container_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // remoteEnv applies to processes the user starts in the container, which for am is
    // the agent itself — so it is folded into the container environment.
    for (k, v) in &resolved.remote_env {
        env.push((k.clone(), v.clone()));
    }

    let mut runtime = crate::container::DevcontainerRuntime {
        env,
        mounts: resolved.mounts.clone(),
        init: resolved.init,
        privileged: false,
        cap_add: Vec::new(),
        security_opt: resolved.security_opt.clone(),
        run_args: Vec::new(),
        workdir: resolved.workspace_folder.clone(),
        entrypoints: resolved.entrypoints.clone(),
        // The spec's default when the key is absent is loginInteractiveShell, so a devcontainer
        // that says nothing still gets the environment its dotfiles set up — which is the whole
        // reason the property exists.
        user_env_probe: UserEnvProbe::parse(resolved.user_env_probe.as_deref()),
        ports: resolved.forward_ports.clone(),
        user: resolved
            .remote_user
            .clone()
            .or_else(|| resolved.container_user.clone()),
    };

    if allow {
        runtime.privileged = resolved.privileged;
        runtime.cap_add = resolved.cap_add.clone();
        runtime.run_args = resolved.run_args.clone();
    } else {
        let note = color::note_prefix(color::enabled(color::Stream::Stderr));
        if resolved.privileged {
            eprintln!(
                "{note} this devcontainer asks for --privileged; am is not granting it. \
                 Set devcontainer.allow_host_commands = true to allow it."
            );
        }
        if !resolved.cap_add.is_empty() {
            eprintln!(
                "{note} not granting capabilities requested by this devcontainer: {}",
                resolved.cap_add.join(", ")
            );
        }
        if !resolved.run_args.is_empty() {
            eprintln!(
                "{note} ignoring runArgs from this devcontainer: {}",
                resolved.run_args.join(" ")
            );
        }
    }
    runtime
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Serialises tests that mutate `AM_DEVCONTAINER_BIN` or exec a mock script.
    ///
    /// The exec case is not paranoia: writing a script and immediately running it races
    /// with any other thread that forks in between, which inherits the still-open write
    /// descriptor and makes the exec fail with `ETXTBSY`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write an executable shell script and return its path.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn fixture(name: &str) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("devcontainer")
            .join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading fixture {}: {e}", path.display()))
    }

    fn ctx() -> SubstitutionContext {
        SubstitutionContext::new(Path::new("/home/dev/.am/worktrees/feat"), "/workspaces/feat")
    }

    // ── Mount normalization ───────────────────────────────────────────────────

    #[test]
    fn normalizes_string_mount() {
        let m = Mount::Str(
            "source=/host/path,target=/mnt/x,type=bind,consistency=cached".to_string(),
        );
        let n = m.normalize().unwrap();
        assert_eq!(n.source.as_deref(), Some("/host/path"));
        assert_eq!(n.target, "/mnt/x");
        assert_eq!(n.kind, "bind");
        assert!(!n.read_only);
    }

    #[test]
    fn normalizes_object_mount() {
        let m: Mount = serde_json_lenient::from_str(
            r#"{"source":"/var/run/docker.sock","target":"/var/run/docker-host.sock","type":"bind"}"#,
        )
        .unwrap();
        let n = m.normalize().unwrap();
        assert_eq!(n.source.as_deref(), Some("/var/run/docker.sock"));
        assert_eq!(n.target, "/var/run/docker-host.sock");
    }

    #[test]
    fn accepts_docker_mount_aliases() {
        let n = Mount::Str("src=/a,dst=/b,ro".to_string()).normalize().unwrap();
        assert_eq!(n.source.as_deref(), Some("/a"));
        assert_eq!(n.target, "/b");
        assert!(n.read_only);
    }

    #[test]
    fn mount_without_target_is_an_error() {
        let err = Mount::Str("source=/a,type=bind".to_string())
            .normalize()
            .unwrap_err();
        assert!(err.to_string().contains("no target"));
    }

    #[test]
    fn mount_defaults_to_bind_when_type_omitted() {
        let n = Mount::Str("source=/a,target=/b".to_string())
            .normalize()
            .unwrap();
        assert_eq!(n.kind, "bind");
    }

    // ── Label parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parses_real_features_label() {
        let snippets = parse_metadata_label(&fixture("features-metadata-label.json")).unwrap();
        assert_eq!(snippets.len(), 6);
        let ids: Vec<_> = snippets.iter().filter_map(|s| s.id.as_deref()).collect();
        assert!(ids.contains(&"ghcr.io/devcontainers/features/docker-outside-of-docker:1"));
        assert!(ids.contains(&"ghcr.io/devcontainers/features/sshd:1"));
    }

    #[test]
    fn accepts_bare_object_label() {
        let snippets = parse_metadata_label(r#"{"remoteUser":"vscode"}"#).unwrap();
        assert_eq!(snippets.len(), 1);
        assert_eq!(snippets[0].remote_user.as_deref(), Some("vscode"));
    }

    #[test]
    fn empty_label_is_not_an_error() {
        assert!(parse_metadata_label("   ").unwrap().is_empty());
    }

    #[test]
    fn ignores_unknown_label_properties() {
        // `customizations` is present in every real label and is none of am's business.
        let snippets =
            parse_metadata_label(r#"[{"customizations":{"vscode":{"extensions":["a.b"]}}}]"#)
                .unwrap();
        assert_eq!(snippets.len(), 1);
    }

    // ── Merge, against the captured label ─────────────────────────────────────

    #[test]
    fn merge_collects_every_feature_entrypoint_in_order() {
        let snippets = parse_metadata_label(&fixture("features-metadata-label.json")).unwrap();
        let resolved = merge(&snippets).unwrap();
        assert_eq!(
            resolved.entrypoints,
            vec![
                "/usr/local/share/docker-init.sh".to_string(),
                "/usr/local/share/ssh-init.sh".to_string(),
            ]
        );
    }

    #[test]
    fn merge_handles_both_mount_shapes_from_one_label() {
        let snippets = parse_metadata_label(&fixture("features-metadata-label.json")).unwrap();
        let resolved = merge(&snippets).unwrap();
        let targets: Vec<_> = resolved.mounts.iter().map(|m| m.target.as_str()).collect();
        assert!(targets.contains(&"/var/run/docker-host.sock")); // object, from a Feature
        assert!(targets.contains(&"/mnt/am-spike")); // string, from devcontainer.json
    }

    #[test]
    fn merge_takes_last_writer_for_scalars() {
        let snippets = parse_metadata_label(&fixture("features-metadata-label.json")).unwrap();
        let resolved = merge(&snippets).unwrap();
        assert_eq!(resolved.remote_user.as_deref(), Some("vscode"));
        assert_eq!(resolved.wait_for.as_deref(), Some("postCreateCommand"));
    }

    #[test]
    fn merge_unions_security_opt_from_features() {
        let snippets = parse_metadata_label(&fixture("features-metadata-label.json")).unwrap();
        let resolved = merge(&snippets).unwrap();
        assert_eq!(resolved.security_opt, vec!["label=disable".to_string()]);
    }

    #[test]
    fn merge_or_s_init_and_privileged() {
        let snippets = parse_metadata_label(&fixture("properties-metadata-label.json")).unwrap();
        let resolved = merge(&snippets).unwrap();
        assert!(resolved.init);
        assert!(resolved.privileged);
        assert_eq!(resolved.cap_add, vec!["SYS_ADMIN".to_string()]);
    }

    #[test]
    fn merge_unions_lists_without_duplicating() {
        let snippets: Vec<MetadataSnippet> = parse_metadata_label(
            r#"[{"capAdd":["SYS_PTRACE"]},{"capAdd":["SYS_PTRACE","SYS_ADMIN"]}]"#,
        )
        .unwrap();
        let resolved = merge(&snippets).unwrap();
        assert_eq!(resolved.cap_add, vec!["SYS_PTRACE", "SYS_ADMIN"]);
    }

    #[test]
    fn later_mount_on_same_target_replaces_earlier() {
        let snippets = parse_metadata_label(
            r#"[{"mounts":["source=/a,target=/same"]},{"mounts":["source=/b,target=/same"]}]"#,
        )
        .unwrap();
        let resolved = merge(&snippets).unwrap();
        assert_eq!(resolved.mounts.len(), 1);
        assert_eq!(resolved.mounts[0].source.as_deref(), Some("/b"));
    }

    #[test]
    fn merge_collects_lifecycle_commands_in_every_shape() {
        let snippets = parse_metadata_label(&fixture("properties-metadata-label.json")).unwrap();
        let resolved = merge(&snippets).unwrap();
        assert_eq!(
            resolved.post_create,
            vec![Command::Argv(vec![
                "echo".to_string(),
                "array-form".to_string()
            ])]
        );
        assert_eq!(
            resolved.post_attach,
            vec![Command::Shell("echo named-form".to_string())]
        );
        assert_eq!(
            resolved.on_create,
            vec![Command::Shell("echo on-create".to_string())]
        );
    }

    // ── The boundary: what the label deliberately drops ───────────────────────
    //
    // These assert *absence*. If a future CLI release starts emitting these properties,
    // these tests fail and am can drop its JSONC parser — that is the point of them.

    #[test]
    fn label_does_not_carry_run_args_or_workspace_folder() {
        let text = fixture("properties-metadata-label.json");
        assert!(
            !text.contains("runArgs"),
            "runArgs appeared in the label; am may no longer need to parse devcontainer.json"
        );
        assert!(!text.contains("workspaceFolder"));
        assert!(!text.contains("workspaceMount"));
    }

    #[test]
    fn label_does_not_carry_initialize_command() {
        // The trust story depends on this: the delegated build never sees a host command.
        let text = fixture("properties-metadata-label.json");
        assert!(!text.contains("initializeCommand"));
    }

    #[test]
    fn config_supplies_what_the_label_drops() {
        let json = parse_config_str(&fixture("properties-devcontainer.json")).unwrap();
        assert_eq!(json.run_args, vec!["--network=host", "--cap-add=SYS_PTRACE"]);
        assert_eq!(json.workspace_folder.as_deref(), Some("/workspaces/custom"));
        assert!(json.initialize_command.is_some());
    }

    // ── JSONC ─────────────────────────────────────────────────────────────────

    #[test]
    fn parses_jsonc_comments_and_trailing_commas() {
        // The features fixture deliberately contains both.
        let json = parse_config_str(&fixture("features-devcontainer.json")).unwrap();
        assert_eq!(json.name.as_deref(), Some("am-spike"));
    }

    // ── Substitution ──────────────────────────────────────────────────────────

    #[test]
    fn substitutes_workspace_variables() {
        let c = ctx();
        assert_eq!(
            c.substitute("${localWorkspaceFolder}/x"),
            "/home/dev/.am/worktrees/feat/x"
        );
        assert_eq!(c.substitute("${localWorkspaceFolderBasename}"), "feat");
        assert_eq!(c.substitute("${containerWorkspaceFolder}"), "/workspaces/feat");
    }

    #[test]
    fn substitutes_local_env() {
        std::env::set_var("AM_TEST_SUBST", "hello");
        assert_eq!(ctx().substitute("${localEnv:AM_TEST_SUBST}"), "hello");
        std::env::remove_var("AM_TEST_SUBST");
    }

    #[test]
    fn unknown_variable_becomes_empty() {
        assert_eq!(ctx().substitute("a${nope}b"), "ab");
    }

    #[test]
    fn unterminated_variable_is_left_alone() {
        // Far more likely to be shell syntax in a lifecycle command than a typo.
        assert_eq!(ctx().substitute("echo ${incomplete"), "echo ${incomplete");
    }

    #[test]
    fn substitution_leaves_plain_text_untouched() {
        assert_eq!(ctx().substitute("no variables here"), "no variables here");
    }

    #[test]
    fn finalize_substitutes_the_label_sourced_mount() {
        // The captured label preserves ${localWorkspaceFolder} verbatim; this is the test
        // that would have caught assuming the CLI had already expanded it.
        let snippets = parse_metadata_label(&fixture("features-metadata-label.json")).unwrap();
        let resolved = merge(&snippets).unwrap();
        let json = parse_config_str(&fixture("features-devcontainer.json")).unwrap();
        let out = finalize(resolved, &json, &ctx());
        let mount = out
            .mounts
            .iter()
            .find(|m| m.target == "/mnt/am-spike")
            .unwrap();
        assert_eq!(
            mount.source.as_deref(),
            Some("/home/dev/.am/worktrees/feat/.am-spike-mount")
        );
    }

    #[test]
    fn finalize_resolves_container_env_references_after_merging() {
        let snippets =
            parse_metadata_label(r#"[{"containerEnv":{"BASE":"/opt"}},{"containerEnv":{"DERIVED":"${containerEnv:BASE}/bin"}}]"#)
                .unwrap();
        let resolved = merge(&snippets).unwrap();
        let out = finalize(resolved, &DevcontainerJson::default(), &ctx());
        assert_eq!(out.container_env.get("DERIVED").unwrap(), "/opt/bin");
    }

    // ── Discovery ─────────────────────────────────────────────────────────────

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn finds_primary_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = tmp.path().join(".devcontainer").join("devcontainer.json");
        write(&expected, "{}");
        assert_eq!(find_config(tmp.path(), None).unwrap(), Some(expected));
    }

    #[test]
    fn primary_config_wins_over_dotfile() {
        let tmp = tempfile::TempDir::new().unwrap();
        let primary = tmp.path().join(".devcontainer").join("devcontainer.json");
        write(&primary, "{}");
        write(&tmp.path().join(".devcontainer.json"), "{}");
        assert_eq!(find_config(tmp.path(), None).unwrap(), Some(primary));
    }

    #[test]
    fn finds_dotfile_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = tmp.path().join(".devcontainer.json");
        write(&expected, "{}");
        assert_eq!(find_config(tmp.path(), None).unwrap(), Some(expected));
    }

    #[test]
    fn finds_single_subfolder_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let expected = tmp
            .path()
            .join(".devcontainer")
            .join("backend")
            .join("devcontainer.json");
        write(&expected, "{}");
        assert_eq!(find_config(tmp.path(), None).unwrap(), Some(expected));
    }

    #[test]
    fn several_subfolder_configs_error_and_list_them() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(
            &tmp.path()
                .join(".devcontainer")
                .join("a")
                .join("devcontainer.json"),
            "{}",
        );
        write(
            &tmp.path()
                .join(".devcontainer")
                .join("b")
                .join("devcontainer.json"),
            "{}",
        );
        let err = find_config(tmp.path(), None).unwrap_err().to_string();
        assert!(err.contains("several devcontainer configs"));
        assert!(err.contains("devcontainer.path"));
    }

    #[test]
    fn no_config_is_not_an_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(find_config(tmp.path(), None).unwrap(), None);
    }

    #[test]
    fn explicit_override_is_used() {
        let tmp = tempfile::TempDir::new().unwrap();
        let custom = tmp.path().join("custom").join("dc.json");
        write(&custom, "{}");
        let found = find_config(tmp.path(), Some(Path::new("custom/dc.json"))).unwrap();
        assert_eq!(found, Some(custom));
    }

    #[test]
    fn missing_override_errors_rather_than_falling_back() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(&tmp.path().join(".devcontainer").join("devcontainer.json"), "{}");
        let err = find_config(tmp.path(), Some(Path::new("nope.json")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("devcontainer.path"));
    }

    // ── Hashing ───────────────────────────────────────────────────────────────

    #[test]
    fn image_name_is_stable_for_identical_config() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("devcontainer.json");
        write(&cfg, r#"{"image":"debian"}"#);
        let a = image_name(&cfg, &[]).unwrap();
        let b = image_name(&cfg, &[]).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("am-dc-"));
    }

    #[test]
    fn image_name_changes_when_config_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("devcontainer.json");
        write(&cfg, r#"{"image":"debian"}"#);
        let before = image_name(&cfg, &[]).unwrap();
        write(&cfg, r#"{"image":"ubuntu"}"#);
        assert_ne!(before, image_name(&cfg, &[]).unwrap());
    }

    #[test]
    fn image_name_changes_when_injected_features_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("devcontainer.json");
        write(&cfg, r#"{"image":"debian"}"#);
        let plain = image_name(&cfg, &[]).unwrap();
        let with = image_name(&cfg, &[InjectedFeature::with_defaults("ghcr.io/x/cc:1")]).unwrap();
        assert_ne!(plain, with);
    }

    #[test]
    fn image_name_changes_when_feature_options_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("devcontainer.json");
        write(&cfg, r#"{"image":"debian"}"#);
        let a = image_name(&cfg, &[InjectedFeature::new("f", "{}")]).unwrap();
        let b = image_name(&cfg, &[InjectedFeature::new("f", r#"{"version":"2"}"#)]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn injected_feature_order_does_not_affect_the_hash() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("devcontainer.json");
        write(&cfg, r#"{"image":"debian"}"#);
        let a = image_name(
            &cfg,
            &[
                InjectedFeature::with_defaults("a"),
                InjectedFeature::with_defaults("b"),
            ],
        )
        .unwrap();
        let b = image_name(
            &cfg,
            &[
                InjectedFeature::with_defaults("b"),
                InjectedFeature::with_defaults("a"),
            ],
        )
        .unwrap();
        assert_eq!(a, b);
    }

    // ── Build command assembly ────────────────────────────────────────────────

    fn build_args(injected: &[InjectedFeature]) -> Vec<String> {
        build_command_args(
            Path::new("/wt"),
            Path::new("/wt/.devcontainer/devcontainer.json"),
            "am-dc-abc123",
            injected,
            Path::new("/usr/bin/podman"),
            false,
        )
    }

    #[test]
    fn build_args_start_with_the_subcommand() {
        // Options placed before `build` are silently ignored by the CLI — the podman
        // path in particular would fall back to docker and fail confusingly.
        assert_eq!(build_args(&[])[0], "build");
    }

    #[test]
    fn build_args_pass_docker_path_after_the_subcommand() {
        let args = build_args(&[]);
        let idx = args.iter().position(|a| a == "--docker-path").unwrap();
        assert!(idx > 0);
        assert_eq!(args[idx + 1], "/usr/bin/podman");
    }

    #[test]
    fn build_args_include_workspace_config_and_image() {
        let args = build_args(&[]);
        let pair = |flag: &str| {
            let i = args.iter().position(|a| a == flag).unwrap();
            args[i + 1].clone()
        };
        assert_eq!(pair("--workspace-folder"), "/wt");
        assert_eq!(
            pair("--config"),
            "/wt/.devcontainer/devcontainer.json"
        );
        assert_eq!(pair("--image-name"), "am-dc-abc123");
    }

    #[test]
    fn build_args_omit_additional_features_when_none_are_injected() {
        assert!(!build_args(&[]).iter().any(|a| a == "--additional-features"));
    }

    #[test]
    fn build_args_render_injected_features_as_json() {
        let args = build_args(&[InjectedFeature::with_defaults(
            "ghcr.io/anthropics/devcontainer-features/claude-code:1",
        )]);
        let i = args.iter().position(|a| a == "--additional-features").unwrap();
        assert_eq!(
            args[i + 1],
            r#"{"ghcr.io/anthropics/devcontainer-features/claude-code:1":{}}"#
        );
    }

    #[test]
    fn feature_options_are_passed_through_as_json() {
        let json = additional_features_json(&[InjectedFeature::new("f", r#"{"version":"20"}"#)]);
        assert_eq!(json, r#"{"f":{"version":"20"}}"#);
    }

    #[test]
    fn malformed_feature_options_fall_back_to_defaults() {
        // A cosmetic config mistake should not abort a multi-minute build.
        let json = additional_features_json(&[InjectedFeature::new("f", "not json")]);
        assert_eq!(json, r#"{"f":{}}"#);
    }

    // ── Running the build ─────────────────────────────────────────────────────

    fn run_build(tmp: &Path, cli_body: &str) -> Result<String> {
        let _g = lock_env();
        let cli = script(tmp, "devcontainer", cli_body);
        build(
            &cli,
            tmp,
            &tmp.join("devcontainer.json"),
            "am-dc-abc123",
            &[],
            Path::new("/usr/bin/podman"),
            false,
        )
    }

    #[test]
    fn build_returns_the_image_name_on_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let image = run_build(
            tmp.path(),
            r#"echo '{"outcome":"success","imageName":["am-dc-abc123"]}'"#,
        )
        .unwrap();
        assert_eq!(image, "am-dc-abc123");
    }

    #[test]
    fn build_surfaces_the_cli_error_message() {
        // The spike's captured failure shape: exit 1 plus an error JSON on stdout.
        let tmp = tempfile::TempDir::new().unwrap();
        let err = run_build(
            tmp.path(),
            r#"echo '{"outcome":"error","message":"Command failed: podman pull nope","description":"An error occurred building the container."}'
exit 1"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("An error occurred building the container"));
        assert!(err.contains("Command failed: podman pull nope"));
    }

    #[test]
    fn build_fails_on_error_outcome_even_when_the_cli_exits_zero() {
        // Defence in depth: don't rely on the exit code alone across CLI versions.
        let tmp = tempfile::TempDir::new().unwrap();
        let err = run_build(
            tmp.path(),
            r#"echo '{"outcome":"error","message":"boom"}'
exit 0"#,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("boom"));
    }

    #[test]
    fn build_fails_on_nonzero_exit_even_without_result_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = run_build(tmp.path(), "exit 3").unwrap_err().to_string();
        assert!(err.contains("exited with"));
    }

    #[test]
    fn build_rejects_a_cli_that_prints_no_result_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = run_build(tmp.path(), "echo hello").unwrap_err().to_string();
        assert!(err.contains("@devcontainers/cli"));
    }

    #[test]
    fn build_ignores_non_json_chatter_before_the_result() {
        // Real runs interleave progress lines; only the last JSON line is the result.
        let tmp = tempfile::TempDir::new().unwrap();
        let image = run_build(
            tmp.path(),
            r#"echo 'Resolving Feature dependencies...'
echo '{"outcome":"success","imageName":["am-dc-abc123"]}'"#,
        )
        .unwrap();
        assert_eq!(image, "am-dc-abc123");
    }

    #[test]
    fn build_passes_its_arguments_to_the_cli() {
        let _g = lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let log = tmp.path().join("args.log");
        let cli = script(
            tmp.path(),
            "devcontainer",
            &format!(
                r#"echo "$@" > {}
echo '{{"outcome":"success","imageName":"am-dc-abc123"}}'"#,
                log.display()
            ),
        );
        build(
            &cli,
            tmp.path(),
            &tmp.path().join("devcontainer.json"),
            "am-dc-abc123",
            &[InjectedFeature::with_defaults("ghcr.io/x/cc:1")],
            Path::new("/usr/bin/podman"),
            false,
        )
        .unwrap();
        let recorded = std::fs::read_to_string(&log).unwrap();
        assert!(recorded.starts_with("build "));
        assert!(recorded.contains("--docker-path /usr/bin/podman"));
        assert!(recorded.contains("ghcr.io/x/cc:1"));
    }

    #[test]
    fn build_accepts_a_string_image_name_as_well_as_an_array() {
        // 0.88 emits an array, but the schema permits a bare string. Both must be read,
        // and the reported name is what am runs — not the one it asked for.
        let tmp = tempfile::TempDir::new().unwrap();
        let from_string = run_build(
            tmp.path(),
            r#"echo '{"outcome":"success","imageName":"reported:tag"}'"#,
        )
        .unwrap();
        assert_eq!(from_string, "reported:tag");

        let from_array = run_build(
            tmp.path(),
            r#"echo '{"outcome":"success","imageName":["reported:tag"]}'"#,
        )
        .unwrap();
        assert_eq!(from_array, "reported:tag");
    }

    #[test]
    fn build_falls_back_to_the_requested_name_when_none_is_reported() {
        let tmp = tempfile::TempDir::new().unwrap();
        let image = run_build(tmp.path(), r#"echo '{"outcome":"success"}'"#).unwrap();
        assert_eq!(image, "am-dc-abc123");
    }

    // ── Locating the CLI ──────────────────────────────────────────────────────

    #[test]
    fn find_cli_uses_the_env_override() {
        let _g = lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let bin = script(tmp.path(), "devcontainer", "exit 0");
        std::env::set_var("AM_DEVCONTAINER_BIN", &bin);

        let found = find_cli("devcontainer");

        std::env::remove_var("AM_DEVCONTAINER_BIN");
        assert_eq!(found.unwrap(), bin);
    }

    #[test]
    fn find_cli_errors_helpfully_when_missing() {
        let _g = lock_env();
        std::env::set_var("AM_DEVCONTAINER_BIN", "/nonexistent/devcontainer");

        let err = find_cli("devcontainer").unwrap_err().to_string();

        std::env::remove_var("AM_DEVCONTAINER_BIN");
        assert!(err.contains("npm install -g @devcontainers/cli"));
        assert!(err.contains("container.mode"));
    }

    // ── Reading the image label ───────────────────────────────────────────────

    #[test]
    fn reads_metadata_label_from_the_runtime() {
        let _g = lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let runtime = script(
            tmp.path(),
            "podman",
            r#"echo '[{"remoteUser":"vscode"},{"entrypoint":"/x.sh"}]'"#,
        );
        let snippets = read_image_metadata(&runtime, "am-dc-abc").unwrap();
        assert_eq!(snippets.len(), 2);
        assert_eq!(snippets[1].entrypoint.as_deref(), Some("/x.sh"));
    }

    #[test]
    fn missing_label_is_empty_not_an_error() {
        // A plain `"image": "..."` config with no Features produces no label at all.
        let _g = lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let runtime = script(tmp.path(), "podman", "echo '<no value>'");
        assert!(read_image_metadata(&runtime, "am-dc-abc").unwrap().is_empty());
    }

    #[test]
    fn inspect_failure_is_reported() {
        let _g = lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let runtime = script(tmp.path(), "podman", "echo 'no such image' >&2\nexit 1");
        let err = read_image_metadata(&runtime, "am-dc-abc")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no such image"));
    }

    #[test]
    fn image_exists_reflects_the_runtime_exit_code() {
        let _g = lock_env();
        let tmp = tempfile::TempDir::new().unwrap();
        let present = script(tmp.path(), "present", "exit 0");
        let absent = script(tmp.path(), "absent", "exit 1");
        assert!(image_exists(&present, "am-dc-abc"));
        assert!(!image_exists(&absent, "am-dc-abc"));
    }

    #[test]
    fn no_cache_flag_is_added_only_when_requested() {
        assert!(!build_args(&[]).iter().any(|a| a == "--no-cache"));
        let rebuilt = build_command_args(
            Path::new("/wt"),
            Path::new("/wt/.devcontainer/devcontainer.json"),
            "am-dc-abc123",
            &[],
            Path::new("/usr/bin/podman"),
            true,
        );
        assert!(rebuilt.iter().any(|a| a == "--no-cache"));
    }

    #[test]
    fn image_name_changes_when_the_dockerfile_changes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("devcontainer.json");
        write(&cfg, r#"{"build":{"dockerfile":"Dockerfile"}}"#);
        let dockerfile = tmp.path().join("Dockerfile");
        write(&dockerfile, "FROM debian\n");
        let before = image_name(&cfg, &[]).unwrap();
        write(&dockerfile, "FROM ubuntu\n");
        assert_ne!(before, image_name(&cfg, &[]).unwrap());
    }

    // ── Unsupported constructs ────────────────────────────────────────────────

    #[test]
    fn forward_ports_accepts_every_form_the_spec_allows() {
        use serde_json::json;
        assert_eq!(ForwardedPort::parse(&json!(3000)), Some(ForwardedPort::Own(3000)));
        assert_eq!(ForwardedPort::parse(&json!("3000")), Some(ForwardedPort::Own(3000)));
        assert_eq!(
            ForwardedPort::parse(&json!("db:5432")),
            Some(ForwardedPort::Service { service: "db".into(), port: 5432 })
        );
        // Not a port at all. Skipped rather than fatal: publishing is a convenience, and
        // refusing to start a session over a malformed entry would be the worse outcome.
        assert_eq!(ForwardedPort::parse(&json!("not-a-port")), None);
        assert_eq!(ForwardedPort::parse(&json!(99999)), None, "outside the u16 range");
        assert_eq!(ForwardedPort::parse(&json!(true)), None);
    }

    #[test]
    fn forwarded_ports_publish_on_loopback() {
        // Not 0.0.0.0: a session container is not something to put on the network by default,
        // and this is what the reference CLI does for a bare appPort.
        assert_eq!(ForwardedPort::publish_spec(3000), "127.0.0.1:3000:3000");
    }

    #[test]
    fn forward_ports_merge_as_a_union_across_snippets() {
        // Two contributors each asking for a port must not cancel each other out, and a port
        // asked for twice is published once.
        let snippets: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"forwardPorts":[3000,"db:5432"]},{"forwardPorts":[3000,8080]}]"#,
        )
        .unwrap();
        let merged = merge(&snippets).unwrap();
        assert_eq!(
            merged.forward_ports,
            vec![
                ForwardedPort::Own(3000),
                ForwardedPort::Service { service: "db".into(), port: 5432 },
                ForwardedPort::Own(8080),
            ]
        );
    }

    #[test]
    fn a_compose_config_naming_its_service_is_supported() {
        let json =
            parse_config_str(r#"{"dockerComposeFile":"docker-compose.yml","service":"app"}"#)
                .unwrap();
        assert!(check_supported(&json).is_ok());
    }

    #[test]
    fn a_compose_config_without_a_service_says_what_to_add() {
        // Without it there is no way to know which container the agent belongs in, and
        // guessing would put it somewhere arbitrary.
        let json = parse_config_str(r#"{"dockerComposeFile":"docker-compose.yml"}"#).unwrap();
        let err = check_supported(&json).unwrap_err().to_string();
        assert!(err.contains("service"), "must name the missing key: {err}");
    }

    #[test]
    fn compose_files_resolve_against_the_config_directory() {
        let json =
            parse_config_str(r#"{"dockerComposeFile":["a.yml","b.yml"],"service":"app"}"#).unwrap();
        let files = compose_files(Path::new("/repo/.devcontainer/devcontainer.json"), &json);
        // Order is preserved: compose layers later files over earlier ones.
        assert_eq!(
            files,
            vec![
                PathBuf::from("/repo/.devcontainer/a.yml"),
                PathBuf::from("/repo/.devcontainer/b.yml"),
            ]
        );
    }

    #[test]
    fn a_config_with_no_compose_file_yields_no_compose_files() {
        let json = parse_config_str(r#"{"image":"debian"}"#).unwrap();
        assert!(compose_files(Path::new("/repo/.devcontainer/devcontainer.json"), &json).is_empty());
    }

    #[test]
    fn plain_image_config_is_supported() {
        let json = parse_config_str(r#"{"image":"debian"}"#).unwrap();
        assert!(check_supported(&json).is_ok());
    }

    // ── Host commands ─────────────────────────────────────────────────────────

    #[test]
    fn initialize_command_is_refused_by_default() {
        // It runs on the host, outside every boundary am exists to provide.
        let json = parse_config_str(r#"{"initializeCommand":"./setup.sh"}"#).unwrap();
        let err = check_host_commands(&json, false).unwrap_err().to_string();
        assert!(err.contains("runs on your host"));
        assert!(err.contains("allow_host_commands"));
    }

    #[test]
    fn initialize_command_is_allowed_when_opted_in() {
        let json = parse_config_str(r#"{"initializeCommand":"./setup.sh"}"#).unwrap();
        assert!(check_host_commands(&json, true).is_ok());
    }

    #[test]
    fn config_without_initialize_command_passes_either_way() {
        let json = parse_config_str(r#"{"image":"debian"}"#).unwrap();
        assert!(check_host_commands(&json, false).is_ok());
    }

    // ── Trust gate ────────────────────────────────────────────────────────────

    fn escalating() -> ResolvedConfig {
        ResolvedConfig {
            init: true,
            privileged: true,
            cap_add: vec!["SYS_ADMIN".to_string()],
            security_opt: vec!["label=disable".to_string()],
            run_args: vec!["--network=host".to_string()],
            ..Default::default()
        }
    }

    fn cfg_with(allow: bool) -> crate::config::Config {
        let mut cfg = crate::config::Config::default();
        cfg.devcontainer.allow_host_commands = allow;
        cfg
    }

    #[test]
    fn trust_gate_drops_escalating_options_by_default() {
        let runtime = apply_trust(&escalating(), &cfg_with(false));
        assert!(!runtime.privileged);
        assert!(runtime.cap_add.is_empty());
        assert!(runtime.run_args.is_empty());
    }

    #[test]
    fn trust_gate_grants_escalating_options_when_opted_in() {
        let runtime = apply_trust(&escalating(), &cfg_with(true));
        assert!(runtime.privileged);
        assert_eq!(runtime.cap_add, vec!["SYS_ADMIN".to_string()]);
        assert_eq!(runtime.run_args, vec!["--network=host".to_string()]);
    }

    #[test]
    fn trust_gate_keeps_non_escalating_settings_either_way() {
        // init and securityOpt come from ordinary Features (sshd, docker-outside-of-docker)
        // and do not hand the container new authority over the host.
        let runtime = apply_trust(&escalating(), &cfg_with(false));
        assert!(runtime.init);
        assert_eq!(runtime.security_opt, vec!["label=disable".to_string()]);
    }

    #[test]
    fn trust_gate_merges_container_and_remote_env() {
        let mut resolved = ResolvedConfig::default();
        resolved
            .container_env
            .insert("A".to_string(), "1".to_string());
        resolved.remote_env.insert("B".to_string(), "2".to_string());
        let runtime = apply_trust(&resolved, &cfg_with(false));
        assert!(runtime.env.contains(&("A".to_string(), "1".to_string())));
        assert!(runtime.env.contains(&("B".to_string(), "2".to_string())));
    }

    // ── Lifecycle hooks ───────────────────────────────────────────────────────

    fn with_hooks() -> ResolvedConfig {
        ResolvedConfig {
            on_create: vec![Command::Shell("echo on-create".to_string())],
            update_content: vec![Command::Shell("echo update".to_string())],
            post_create: vec![Command::Argv(vec![
                "npm".to_string(),
                "install".to_string(),
            ])],
            post_start: vec![Command::Shell("echo start".to_string())],
            post_attach: vec![Command::Shell("echo attach".to_string())],
            ..Default::default()
        }
    }

    #[test]
    fn hooks_run_in_spec_order() {
        let (snippets, _) = startup_commands(&with_hooks(), false);
        assert_eq!(
            snippets,
            vec!["echo on-create", "echo update", "npm install", "echo start"]
        );
    }

    #[test]
    fn post_attach_is_not_run() {
        // `am attach` moves tmux focus; it never attaches to the container, so there is
        // no attach event to hang this off.
        let (snippets, names) = startup_commands(&with_hooks(), false);
        assert!(!snippets.iter().any(|s| s.contains("attach")));
        assert!(!names.iter().any(|n| n == "postAttachCommand"));
    }

    #[test]
    fn hooks_are_recorded_by_name() {
        let (_, names) = startup_commands(&with_hooks(), false);
        assert_eq!(
            names,
            vec![
                "onCreateCommand",
                "updateContentCommand",
                "postCreateCommand",
                "postStartCommand"
            ]
        );
    }

    #[test]
    fn skip_lifecycle_suppresses_every_hook() {
        let (snippets, names) = startup_commands(&with_hooks(), true);
        assert!(snippets.is_empty());
        assert!(names.is_empty());
    }

    #[test]
    fn a_stage_with_no_commands_is_not_recorded() {
        let resolved = ResolvedConfig {
            post_create: vec![Command::Shell("echo only".to_string())],
            ..Default::default()
        };
        let (_, names) = startup_commands(&resolved, false);
        assert_eq!(names, vec!["postCreateCommand"]);
    }

    #[test]
    fn shell_hooks_keep_their_shell_syntax() {
        // A shell-form command may legitimately contain && or a pipe; quoting it would
        // turn a working hook into a command named "a && b".
        let cmd = Command::Shell("apt-get update && apt-get install -y jq".to_string());
        assert_eq!(cmd.to_shell(), "apt-get update && apt-get install -y jq");
    }

    #[test]
    fn argv_hooks_are_quoted_rather_than_interpreted() {
        let cmd = Command::Argv(vec!["echo".to_string(), "two words".to_string()]);
        assert_eq!(cmd.to_shell(), "echo 'two words'");
    }
}
