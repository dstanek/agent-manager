//! Dev Container support: discovery, parsing, and configuration merge.
//!
//! `am` builds the image itself (`native/`) and runs it itself (`container.rs`,
//! `compose.rs`); see `specs/devcontainer-support.md`. This module owns everything needed to
//! turn a repo's `.devcontainer/devcontainer.json` plus the built image's
//! `devcontainer.metadata` label into a `ResolvedConfig` that the run path can use.
//!
//! Two sources feed the merge, and the split between them is not arbitrary — it is dictated
//! by what goes into the label, which is defined by the spec and pinned against the reference
//! implementation by the differential tests:
//!
//! 1. The **label** carries feature contributions *and* the whole `devcontainer.json`
//!    reduced to the metadata schema. Elements are already in merge order (base image, then
//!    each Feature, then the config), so a left-to-right fold gets precedence right.
//! 2. **`devcontainer.json`** is parsed here only for the handful of properties the metadata
//!    schema drops: `runArgs`, `workspaceFolder`, `workspaceMount`, `initializeCommand`,
//!    `dockerComposeFile`, and `name`.
//!
//! Fixtures captured from real reference-CLI runs live in `tests/fixtures/devcontainer/`.
//! They are how `am`'s builder is kept honest; the CLI itself is not used at runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::color;

use crate::error::AmError;

pub mod lock;
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

/// A shell snippet running every member of a named lifecycle group concurrently.
///
/// Each member is backgrounded and every pid is waited on individually, so the group fails if
/// *any* member fails — a bare `wait` reports only the last one, which would let a failed setup
/// step pass unnoticed.
fn parallel_script(map: &BTreeMap<String, NamedCommand>) -> String {
    let mut script = String::from("__am_pids=''\n");
    for value in map.values() {
        let command = match value {
            NamedCommand::Shell(s) => s.clone(),
            NamedCommand::Argv(a) => a
                .iter()
                .map(|arg| crate::command::shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" "),
        };
        script.push_str(&format!("{{ {command} ; }} &\n__am_pids=\"$__am_pids $!\"\n"));
    }
    script.push_str(
        "__am_rc=0\n\
         for __am_p in $__am_pids; do wait \"$__am_p\" || __am_rc=1; done\n\
         unset __am_pids __am_p\n\
         [ \"$__am_rc\" -eq 0 ]",
    );
    script
}

impl LifecycleCommand {
    /// Flatten into the individual commands to run, in a stable order.
    ///
    /// A named group becomes **one** command that runs its members in parallel, which is the
    /// whole reason the object form exists. Running them in sequence looks equivalent for
    /// independent commands and deadlocks for co-dependent ones: `{ "server": "npm start",
    /// "wait": "wait-on http://localhost:3000 && npm run seed" }` never gets past the server,
    /// because it is not supposed to exit.
    ///
    /// Members are ordered by key so the generated script is reproducible; they run
    /// concurrently, so the order is presentational.
    pub fn commands(&self) -> Vec<Command> {
        match self {
            LifecycleCommand::Shell(s) => vec![Command::Shell(s.clone())],
            LifecycleCommand::Argv(a) => vec![Command::Argv(a.clone())],
            LifecycleCommand::Named(map) if map.len() > 1 => {
                vec![Command::Shell(parallel_script(map))]
            }
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
    // The object form accepts the same field names as the string form below, because the
    // reference CLI copies a config's mount into the label *verbatim* — a config written as
    // `{"type":"volume","src":"v","dst":"/v"}` reaches this parser exactly as written, and
    // without the aliases it fails to deserialize and takes the whole label with it.
    #[serde(default, alias = "src")]
    pub source: Option<String>,
    #[serde(alias = "dst", alias = "destination")]
    pub target: String,
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default, alias = "ro")]
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
    /// `null` is a permitted value in the schema and means *unset*, so this holds options
    /// rather than strings. Modelling it as `String` made any label carrying one unparseable,
    /// which surfaced only after the build as "parsing the devcontainer.metadata image label".
    #[serde(default)]
    pub remote_env: BTreeMap<String, Option<String>>,
    pub container_user: Option<String>,
    pub remote_user: Option<String>,
    pub user_env_probe: Option<String>,
    pub override_command: Option<bool>,
    #[serde(default)]
    pub update_remote_user_uid: Option<bool>,
    /// What should happen to the environment when the session ends.
    #[serde(default)]
    pub shutdown_action: Option<String>,
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
    /// Per-port settings, keyed by port number or `"<lo>-<hi>"` range.
    #[serde(default)]
    pub ports_attributes: BTreeMap<String, PortAttributes>,
    /// The same settings for every port no key matches.
    pub other_ports_attributes: Option<PortAttributes>,
}

/// The settings a config can attach to a forwarded port.
///
/// Most of this property describes a port to an *editor* — what to label it, whether to open a
/// browser when it appears — and `am` has no editor, so those fields are not modelled. One is
/// not editor-specific: `onAutoForward: "ignore"` says this port should not be forwarded at all,
/// which in am's model means not published. Honouring it matters because the usual reason to
/// write it is that something else on the host already owns that port.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PortAttributes {
    pub on_auto_forward: Option<String>,
}

impl PortAttributes {
    fn is_ignored(&self) -> bool {
        self.on_auto_forward.as_deref() == Some("ignore")
    }
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
    /// An already-rendered publish specification, from `appPort`.
    Published(String),
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

    /// The publish specification for this port, if it has one on a single container.
    pub fn spec(&self) -> Option<String> {
        match self {
            ForwardedPort::Own(port) => Some(Self::publish_spec(*port)),
            ForwardedPort::Published(spec) => Some(spec.clone()),
            // Names another compose service, so there is nothing to publish here.
            ForwardedPort::Service { .. } => None,
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
    /// Services to start. Empty means all of them, which is the spec's default.
    #[serde(default)]
    pub run_services: Vec<String>,
    /// Ports to publish. Unlike `forwardPorts` this is a *publish* instruction — the reference
    /// CLI turns it into `-p` — and it is a `devcontainer.json` property, not label-carried.
    #[serde(default)]
    pub app_port: AppPort,
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
    /// Extra flags passed to the build command verbatim. Part of the spec, and the only way
    /// to reach a runtime feature `am` does not model.
    #[serde(default)]
    pub options: Vec<String>,
    /// Images to consider as cache sources.
    #[serde(default)]
    pub cache_from: CacheFrom,
}

/// `appPort` is a number, a string, or an array mixing both.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum AppPort {
    #[default]
    None,
    // `Many` is tried before `One` deliberately: `serde_json::Value` matches an array too, so
    // the scalar variant would swallow the list and yield nothing.
    Many(Vec<serde_json::Value>),
    One(serde_json::Value),
}

impl AppPort {
    /// The publish specifications, as the runtime's `-p` expects them.
    ///
    /// A bare port binds loopback — which is what the reference CLI does, verified by reading
    /// the `docker run` line it builds — while an explicit `host:container` mapping is passed
    /// through as written, since spelling it out is how a config asks for something wider.
    pub fn publish_specs(&self) -> Vec<String> {
        let values = match self {
            AppPort::None => Vec::new(),
            AppPort::One(v) => vec![v.clone()],
            AppPort::Many(v) => v.clone(),
        };
        values
            .iter()
            .filter_map(|value| match value {
                serde_json::Value::Number(n) => n.as_u64().map(|p| format!("127.0.0.1:{p}:{p}")),
                serde_json::Value::String(s) if s.contains(':') => Some(s.clone()),
                serde_json::Value::String(s) => {
                    s.parse::<u16>().ok().map(|p| format!("127.0.0.1:{p}:{p}"))
                }
                _ => None,
            })
            .collect()
    }
}

/// `cacheFrom` is a string or an array of them.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum CacheFrom {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl CacheFrom {
    pub fn images(&self) -> Vec<String> {
        match self {
            CacheFrom::None => Vec::new(),
            CacheFrom::One(s) => vec![s.clone()],
            CacheFrom::Many(v) => v.clone(),
        }
    }
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
    /// The environment of the *container*, which is the image's own plus whatever the config
    /// contributes — not merely the config's `containerEnv`. `${containerEnv:PATH}` is a
    /// documented idiom and the image is the only place `PATH` is defined.
    pub container_env: BTreeMap<String, String>,
    /// Stable, host-unique identifier for this dev container.
    pub devcontainer_id: Option<String>,
}

impl SubstitutionContext {
    pub fn new(local_workspace_folder: &Path, container_workspace_folder: &str) -> Self {
        Self {
            local_workspace_folder: local_workspace_folder.to_path_buf(),
            container_workspace_folder: container_workspace_folder.to_string(),
            container_env: BTreeMap::new(),
            devcontainer_id: None,
        }
    }

    /// Set the identifier `${devcontainerId}` expands to.
    ///
    /// The spec requires it be unique among dev containers on the same host and stable across
    /// rebuilds. Features use it to name volumes — `docker-in-docker` asks for
    /// `dind-var-lib-docker-${devcontainerId}` — so leaving it empty makes every session on the
    /// machine share one volume, which is the exact collision the variable exists to prevent.
    pub fn with_devcontainer_id(mut self, id: impl Into<String>) -> Self {
        self.devcontainer_id = Some(id.into());
        self
    }

    fn basename(path: &Path) -> String {
        path.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Resolve a single `${...}` body, or `None` to leave it as written.
    ///
    /// A variable this does not recognise is left **literal**, which is what the reference
    /// implementation does — every unmatched branch there returns the original text. Collapsing
    /// it to an empty string is worse than doing nothing: `${devcontainerId}` became `""` and
    /// silently merged every session's volumes, and an unsupported variable is indistinguishable
    /// from one that legitimately expands to nothing.
    ///
    /// The one empty-string case is the spec's own: an environment variable that is not set and
    /// carries no default.
    fn resolve(&self, name: &str) -> Option<String> {
        // `env:` is an accepted alias for `localEnv:`.
        for prefix in ["localEnv:", "env:"] {
            if let Some(rest) = name.strip_prefix(prefix) {
                let (var, default) = Self::split_default(rest);
                return Some(std::env::var(var).unwrap_or_else(|_| default.to_string()));
            }
        }
        if let Some(rest) = name.strip_prefix("containerEnv:") {
            let (var, default) = Self::split_default(rest);
            return Some(
                self.container_env
                    .get(var)
                    .cloned()
                    .unwrap_or_else(|| default.to_string()),
            );
        }
        match name {
            "localWorkspaceFolder" => {
                Some(self.local_workspace_folder.to_string_lossy().into_owned())
            }
            "containerWorkspaceFolder" => Some(self.container_workspace_folder.clone()),
            "localWorkspaceFolderBasename" => Some(Self::basename(&self.local_workspace_folder)),
            "containerWorkspaceFolderBasename" => {
                Some(Self::basename(Path::new(&self.container_workspace_folder)))
            }
            // Left literal until it is known, exactly as the CLI does, so a caller that has not
            // supplied one cannot silently produce a shared name.
            "devcontainerId" => self.devcontainer_id.clone(),
            _ => None,
        }
    }

    /// Split `VAR:default` into its parts. A missing default is the empty string, which is what
    /// an unset variable expands to.
    fn split_default(rest: &str) -> (&str, &str) {
        match rest.split_once(':') {
            Some((var, default)) => (var, default),
            None => (rest, ""),
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
                    let name = &after[..end];
                    match self.resolve(name) {
                        Some(value) => out.push_str(&value),
                        None => out.push_str(&format!("${{{name}}}")),
                    }
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
    /// `null` means the variable is deliberately *not* set, so the value is an option.
    pub remote_env: BTreeMap<String, Option<String>>,
    pub container_user: Option<String>,
    pub remote_user: Option<String>,
    pub user_env_probe: Option<String>,
    pub override_command: Option<bool>,
    /// Whether the container user's UID/GID should follow the host's. Defaults to true on
    /// Linux per the spec, which is what makes a bind-mounted worktree writable.
    pub update_remote_user_uid: Option<bool>,
    /// `"none"` | `"stopContainer"` | `"stopCompose"`. Absent means the mode's default.
    pub shutdown_action: Option<String>,
    pub wait_for: Option<String>,
    pub on_create: Vec<Command>,
    pub update_content: Vec<Command>,
    pub post_create: Vec<Command>,
    pub post_start: Vec<Command>,
    pub post_attach: Vec<Command>,
    /// Ports to publish, in contribution order and de-duplicated.
    pub forward_ports: Vec<ForwardedPort>,
    /// Per-port settings, merged key by key.
    pub ports_attributes: BTreeMap<String, PortAttributes>,
    pub other_ports_attributes: Option<PortAttributes>,
    /// From `devcontainer.json` only — the label drops these.
    pub run_args: Vec<String>,
    /// `appPort`, already rendered as publish specifications.
    pub app_port: Vec<String>,
    pub workspace_folder: Option<String>,
    pub workspace_mount: Option<String>,
    pub name: Option<String>,
}

impl ResolvedConfig {
    /// Whether `portsAttributes` asks that this port not be forwarded.
    ///
    /// The keyed entry wins; `otherPortsAttributes` covers everything no key matches.
    pub fn port_is_ignored(&self, port: u16) -> bool {
        match self.ports_attributes.iter().find(|(key, _)| port_key_matches(key, port)) {
            Some((_, attrs)) => attrs.is_ignored(),
            None => self.other_ports_attributes.as_ref().is_some_and(PortAttributes::is_ignored),
        }
    }
}

/// A `portsAttributes` key: a port number, or an inclusive `"<lo>-<hi>"` range.
///
/// VS Code also accepts a regex key. `am` treats one as matching nothing, so such a port falls
/// through to `otherPortsAttributes` — the same place it would land if the key were absent.
fn port_key_matches(key: &str, port: u16) -> bool {
    if let Ok(exact) = key.trim().parse::<u16>() {
        return exact == port;
    }
    match key.split_once('-') {
        Some((lo, hi)) => match (lo.trim().parse::<u16>(), hi.trim().parse::<u16>()) {
            (Ok(lo), Ok(hi)) => (lo..=hi).contains(&port),
            _ => false,
        },
        None => false,
    }
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
            // A later `null` unsets an earlier value, which is what the null is for.
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
        if snippet.update_remote_user_uid.is_some() {
            out.update_remote_user_uid = snippet.update_remote_user_uid;
        }
        if snippet.shutdown_action.is_some() {
            out.shutdown_action = snippet.shutdown_action.clone();
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
        // Keyed settings merge per key rather than wholesale, so a config that describes one
        // port does not discard what a Feature said about another.
        for (k, v) in &snippet.ports_attributes {
            out.ports_attributes.insert(k.clone(), v.clone());
        }
        if snippet.other_ports_attributes.is_some() {
            out.other_ports_attributes = snippet.other_ports_attributes.clone();
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
    resolved.app_port = json.app_port.publish_specs();
    resolved.workspace_folder = json.workspace_folder.clone();
    resolved.workspace_mount = json.workspace_mount.clone();
    resolved.name = json.name.clone();

    let mut ctx = ctx.clone();
    // The config's own contributions win over the image's, since they will be set on the
    // container; everything else the image defines stays visible to `${containerEnv:…}`.
    for (key, value) in &resolved.container_env {
        ctx.container_env.insert(key.clone(), value.clone());
    }

    resolved.container_env = resolved
        .container_env
        .iter()
        .map(|(k, v)| (k.clone(), ctx.substitute(v)))
        .collect();
    resolved.remote_env = resolved
        .remote_env
        .iter()
        .map(|(k, v)| (k.clone(), v.as_deref().map(|value| ctx.substitute(value))))
        .collect();
    for mount in &mut resolved.mounts {
        mount.source = mount.source.as_deref().map(|s| ctx.substitute(s));
        mount.target = ctx.substitute(&mount.target);
    }
    for arg in &mut resolved.run_args {
        *arg = ctx.substitute(arg);
    }
    // Feature entrypoints are listed in the spec as supporting `${devcontainerId}`, and they
    // are shell commands that end up in the container's command line.
    for entrypoint in &mut resolved.entrypoints {
        *entrypoint = ctx.substitute(entrypoint);
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
        hash_build_context(&mut hasher, config_path, &json);
    }

    // A Feature vendored in the repo is build input like the Dockerfile is: editing its
    // install.sh must produce a different image, or `am start` reuses a stale one and the edit
    // appears to do nothing. Registry and tarball Features are *not* covered — resolving those
    // means a network round trip, and doing one per `am start` would undo the whole point of
    // hashing. `--rebuild` remains the answer when a moving tag has moved.
    let base = config_path.parent().unwrap_or(Path::new("."));
    for id in local_feature_ids(&String::from_utf8_lossy(&bytes)) {
        hasher.update(id.as_bytes());
        hash_dir(&mut hasher, &base.join(&id));
    }

    // Registry and tarball Features reach the hash through the lockfile. Hashing them
    // directly would mean a network round trip per `am start` just to discover that nothing
    // moved; the lockfile records what they last resolved to, so a moved tag changes this file,
    // which changes the image name, which rebuilds. Nothing is fetched on the fast path.
    hasher.update(lock::load(config_path).canonical().as_bytes());

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

/// Fold the build context's *tracked* files into the hash.
///
/// A `COPY` in the Dockerfile makes those files build input, so editing one has to produce a
/// different image — otherwise `am start` reuses the old one and the edit appears to do nothing,
/// which is a genuinely baffling way to lose an afternoon.
///
/// Only files git knows about, and only their names and contents. That bound is what makes this
/// affordable: `"context": ".."` means the whole repository, and walking it would drag in
/// `target/`, `node_modules/` and every build artefact — exactly the paths a `.dockerignore`
/// excludes and `git` already ignores. It also makes the hash reproducible across machines,
/// where mtimes and directory order are not.
///
/// A context outside a repository, or a `git` that fails, contributes nothing: the previous
/// behaviour, and `--rebuild` remains the answer.
fn hash_build_context(hasher: &mut sha2::Sha256, config_path: &Path, json: &DevcontainerJson) {
    use sha2::Digest;

    let base = config_path.parent().unwrap_or(Path::new("."));
    let context = json
        .build
        .as_ref()
        .and_then(|b| b.context.as_deref())
        .map(|c| base.join(c))
        .unwrap_or_else(|| base.to_path_buf());

    let Ok(output) = std::process::Command::new("git")
        .arg("-C")
        .arg(&context)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }

    // `ls-files` output is already sorted, and NUL-separated so a filename containing a newline
    // does not split into two.
    for name in output.stdout.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        hasher.update(name);
        let path = context.join(String::from_utf8_lossy(name).as_ref());
        if let Ok(content) = std::fs::read(&path) {
            hasher.update(&content);
        }
    }
}

/// The `./path` Feature ids a config names, in sorted order.
///
/// Read from the raw JSON because `features` is deliberately not modelled — the builder passes
/// the whole object through, so there is no typed field to read here.
fn local_feature_ids(text: &str) -> Vec<String> {
    let Ok(raw) = serde_json_lenient::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = raw
        .get("features")
        .and_then(serde_json::Value::as_object)
        .map(|m| {
            m.keys()
                .filter(|id| id.starts_with("./") || id.starts_with("../"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids
}

/// Fold a directory's contents into `hasher`, deterministically.
///
/// Names as well as bytes, so deleting a file changes the hash. A path that cannot be read is
/// folded in as its name: a missing local Feature is the build step's error to report clearly,
/// not this function's to raise obscurely.
fn hash_dir(hasher: &mut sha2::Sha256, dir: &Path) {
    use sha2::Digest;
    let Ok(entries) = std::fs::read_dir(dir) else {
        hasher.update(dir.to_string_lossy().as_bytes());
        return;
    };
    let mut paths: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        hasher.update(path.file_name().unwrap_or_default().as_encoded_bytes());
        if path.is_dir() {
            hash_dir(hasher, &path);
        } else if let Ok(content) = std::fs::read(&path) {
            hasher.update(&content);
        }
    }
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
pub fn build_image(req: &BuildRequest, runtime_bin: &Path) -> Result<String> {
    // The builder needs the config as raw JSON: the properties it copies into the metadata
    // label are deliberately not modelled, so they can pass through untouched.
    let text = std::fs::read_to_string(req.config_path)
        .with_context(|| format!("reading {}", req.config_path.display()))?;
    let raw: serde_json::Value = serde_json_lenient::from_str(&text)
        .map_err(|e| AmError::ConfigError(e.to_string()))?;

    native::build(req, runtime_bin, &raw)
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

/// The environment a built image declares, as a map.
///
/// This is what `${containerEnv:VAR}` resolves against: the variable names a value in the
/// *container's* environment, and for everything the config does not set itself the image is
/// where that value comes from. Resolving `${containerEnv:PATH}` from the config's own
/// `containerEnv` — which almost never defines `PATH` — produced an empty string, and the
/// documented idiom `"PATH": "${containerEnv:PATH}:/extra"` then *replaced* the image's `PATH`
/// with `:/extra`.
pub fn image_env(runtime_bin: &Path, image: &str) -> BTreeMap<String, String> {
    let Ok(output) = std::process::Command::new(runtime_bin)
        .args(["inspect", image, "--format", "{{ json .Config.Env }}"])
        .output()
    else {
        return BTreeMap::new();
    };
    if !output.status.success() {
        return BTreeMap::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<Vec<String>>(text.trim())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default()
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

/// `postAttachCommand`, as shell snippets.
///
/// Separate from [`startup_commands`] because it is the one hook that is not tied to creating or
/// starting the container: the spec runs it every time a tool attaches. `am` therefore reaches it
/// from two places — chained ahead of the agent when a session is started or its container
/// recreated, and `exec`'d into an already-running container when `am attach` finds one. Keeping
/// it out of the startup list is also what stops it being recorded in `lifecycle_done`, which
/// tracks hooks that are meant to run once per container.
pub fn attach_commands(resolved: &ResolvedConfig, skip: bool) -> Vec<String> {
    if skip {
        return Vec::new();
    }
    resolved.post_attach.iter().map(Command::to_shell).collect()
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

/// Resolve `path` to a canonical form for a trust decision, even when it (or a trailing part
/// of it) does not exist yet.
///
/// `Path::canonicalize` requires the path to exist, but a bind mount source is not required
/// to — the runtime creates a missing directory on start, and a config asking for a fresh
/// subdirectory under the worktree is ordinary, not suspicious. So a non-existent source is
/// not automatically *safe* either: walk up to the nearest existing ancestor, canonicalize
/// that (resolving any symlink placed there), then re-append the still-missing suffix
/// literally. Nothing below an existing ancestor can be a symlink, because the filesystem has
/// nothing there yet, so this is exact rather than a heuristic.
///
/// Returns `None` when even that walk cannot pin the path down (for example a path built so
/// that a literal `..` survives past the point anything on disk exists). That failure is
/// treated as untrusted by the caller — the safe direction, since the alternative is guessing.
fn resolve_for_trust(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    let mut suffix: Vec<&std::ffi::OsStr> = Vec::new();
    while !current.exists() {
        let name = current.file_name()?;
        suffix.push(name);
        current = current.parent()?;
    }
    let mut resolved = current.canonicalize().ok()?;
    for name in suffix.into_iter().rev() {
        resolved.push(name);
    }
    Some(resolved)
}

/// Whether a repo-declared bind mount's source may be honoured without
/// `devcontainer.allow_host_commands`.
///
/// Trusted means its canonical location sits inside the session worktree — `am`'s own mounts
/// (worktree, VCS data, credentials) do not go through this at all, so this is only ever
/// judging a path a repository asked for.
fn bind_source_is_trusted(source: &str, worktree_canonical: &Path) -> bool {
    match resolve_for_trust(Path::new(source)) {
        Some(resolved) => resolved.starts_with(worktree_canonical),
        None => false,
    }
}

/// Drop repo-declared bind mounts whose source resolves outside the session worktree, unless
/// the user has opted in.
///
/// Named volumes and tmpfs mounts name no host path, so they carry no risk and always pass
/// through. A repository config is untrusted input — the same reason `initializeCommand` is
/// refused by [`check_host_commands`] and `privileged`/`capAdd`/`runArgs`/`securityOpt` are
/// gated just below — and an arbitrary bind mount is a direct escape from the isolation `am`
/// exists to provide. Dropping rather than refusing to start matches how the other escalating
/// options are handled: most configs still work without a mount they didn't strictly need.
fn filter_untrusted_mounts(
    mounts: Vec<NormalizedMount>,
    worktree: &Path,
    allow: bool,
) -> Vec<NormalizedMount> {
    if allow {
        return mounts;
    }
    // Mirrors `resolve_for_trust`'s own fallback: a worktree that cannot be canonicalized
    // (a test fixture that never touches disk, say) is compared literally rather than
    // treated as an automatic pass or fail.
    let worktree_canonical = resolve_for_trust(worktree).unwrap_or_else(|| worktree.to_path_buf());
    let note = color::note_prefix(color::enabled(color::Stream::Stderr));
    mounts
        .into_iter()
        .filter(|m| {
            if m.kind != "bind" {
                return true;
            }
            // A source-less bind is meaningless and already dropped downstream; nothing to
            // judge here.
            let Some(source) = &m.source else { return true };
            if bind_source_is_trusted(source, &worktree_canonical) {
                return true;
            }
            eprintln!(
                "{note} not mounting {source} from this devcontainer — it is outside the \
                 session worktree. Set devcontainer.allow_host_commands = true if you trust \
                 this repository's config and want to allow it."
            );
            false
        })
        .collect()
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
    worktree: &Path,
) -> crate::container::DevcontainerRuntime {
    let allow = cfg.devcontainer.allow_host_commands;
    let mut env: Vec<(String, String)> = resolved
        .container_env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    // remoteEnv applies to processes the user starts in the container, which for am is
    // the agent itself — so it is folded into the container environment. A `null` value means
    // the variable is deliberately not set, so it contributes nothing rather than an empty
    // string: `FOO=` and "no FOO" are different to a program that checks for presence.
    for (k, v) in &resolved.remote_env {
        if let Some(value) = v {
            env.push((k.clone(), value.clone()));
        }
    }

    // `workspaceMount` was parsed, substituted, and then never used — so a config pairing it
    // with `workspaceFolder` got `--workdir` pointing at a path nothing was mounted at, and the
    // agent started in an empty root-owned directory.
    //
    // It is added *alongside* am's host-path mirroring rather than replacing it. The mirroring
    // is what makes a git worktree's absolute `gitdir:` pointer and a jj workspace's relative
    // repo path resolve; dropping it to honour the config would trade one broken thing for
    // another. Both paths are the same bind, so an edit through either is the same file.
    let mut mounts = resolved.mounts.clone();
    if let Some(spec) = &resolved.workspace_mount {
        match (Mount::Str(spec.clone())).normalize() {
            Ok(normalized) => {
                if !mounts.iter().any(|m| m.target == normalized.target) {
                    mounts.push(normalized);
                }
            }
            Err(e) => eprintln!(
                "{} ignoring workspaceMount: {e}",
                color::warning_prefix(color::enabled(color::Stream::Stderr))
            ),
        }
    }
    // Applied after workspaceMount is folded in, so a substituted external path cannot use it
    // as a loophole around the same policy every other repo-declared bind mount is subject to.
    let mounts = filter_untrusted_mounts(mounts, worktree, allow);

    let mut runtime = crate::container::DevcontainerRuntime {
        env,
        mounts,
        init: resolved.init,
        privileged: false,
        cap_add: Vec::new(),
        security_opt: Vec::new(),
        run_args: Vec::new(),
        workdir: resolved.workspace_folder.clone(),
        entrypoints: resolved.entrypoints.clone(),
        // A Feature entrypoint runs as the container user, so when there is one the container
        // starts as that user and the agent is dropped to `remoteUser` afterwards. With no
        // entrypoint there is nothing needing elevation, so the container runs as the remote
        // user directly — which is observationally the same and keeps the common path simple.
        drop_to: if resolved.entrypoints.is_empty() {
            None
        } else {
            resolved.remote_user.clone().filter(|remote| {
                resolved.container_user.as_deref() != Some(remote.as_str())
            })
        },
        // Defaults to true per the spec: on Linux the container user's UID/GID follow the
        // host's, which is what keeps a bind-mounted worktree writable.
        update_remote_user_uid: resolved.update_remote_user_uid.unwrap_or(true),
        // The spec's default when the key is absent is loginInteractiveShell, so a devcontainer
        // that says nothing still gets the environment its dotfiles set up — which is the whole
        // reason the property exists.
        user_env_probe: UserEnvProbe::parse(resolved.user_env_probe.as_deref()),
        // `appPort` publishes alongside `forwardPorts`. They are different properties — one
        // asks a tool to forward, the other asks the runtime to publish — and a config may use
        // either or both.
        // `portsAttributes` can say `onAutoForward: "ignore"` for a port, which is the one part
        // of that property that is not about an editor: it asks that the port not be forwarded.
        // It applies to `forwardPorts` only — `appPort` asks the runtime to publish directly,
        // and is not something an editor was ever going to forward.
        ports: resolved
            .forward_ports
            .iter()
            .filter(|port| !matches!(port, ForwardedPort::Own(n) if resolved.port_is_ignored(*n)))
            .cloned()
            .chain(resolved.app_port.iter().map(|spec| ForwardedPort::Published(spec.clone())))
            .collect(),
        // With an entrypoint to run, this is who the *container* starts as — the container
        // user, or the image's own default when the config names none. Otherwise it is the
        // remote user, as before.
        user: if resolved.entrypoints.is_empty() {
            resolved.remote_user.clone().or_else(|| resolved.container_user.clone())
        } else {
            resolved.container_user.clone()
        },
    };

    // `overrideCommand: false` asks that the image's own command run — which in am's model is
    // impossible, because the agent *is* the container command. Saying so beats ignoring it:
    // the property is usually set because an init process matters.
    if resolved.override_command == Some(false) {
        eprintln!(
            "{} this devcontainer sets overrideCommand: false, but am runs the agent as the \
             container's command — the image's own command will not run",
            color::note_prefix(color::enabled(color::Stream::Stderr))
        );
    }

    if allow {
        runtime.privileged = resolved.privileged;
        runtime.cap_add = resolved.cap_add.clone();
        runtime.security_opt = resolved.security_opt.clone();
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
        if !resolved.security_opt.is_empty() {
            eprintln!(
                "{note} not granting security options requested by this devcontainer: {}",
                resolved.security_opt.join(", ")
            );
        }
        if !resolved.run_args.is_empty() {
            eprintln!(
                "{note} ignoring runArgs from this devcontainer: {}",
                resolved.run_args.join(" ")
            );
        }
    }

    // Ports last, so the check runs against the list the container would actually be started
    // with. A port another process already holds would otherwise fail the whole `run`.
    let busy = crate::container::drop_busy_ports(&mut runtime.ports);
    if !busy.is_empty() {
        eprintln!(
            "{} not forwarding {} — already in use on this host. \
             The session runs without {}; another am session on this repo forwards the same \
             ports, and whatever is listening is reachable there.",
            color::note_prefix(color::enabled(color::Stream::Stderr)),
            busy.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", "),
            if busy.len() == 1 { "it" } else { "them" },
        );
    }

    runtime
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// Serialises tests that mutate the environment or exec a mock script.
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
    fn an_unknown_variable_is_left_literal() {
        // Not empty. Every unmatched branch in the reference implementation returns the
        // original text, and collapsing instead is actively harmful: `${devcontainerId}` became
        // `""`, so every docker-in-docker session on a host silently shared one volume.
        assert_eq!(ctx().substitute("a${nope}b"), "a${nope}b");
    }

    #[test]
    fn a_workspace_folder_is_substituted_before_it_becomes_the_container_path() {
        // `/workspaces/${localWorkspaceFolderBasename}` is the common spelling, and
        // `substitute` does not re-scan its own output — so a context built from the raw string
        // leaves an unexpanded `${…}` inside every `${containerWorkspaceFolder}`.
        let bare = SubstitutionContext::new(Path::new("/home/dev/.am/worktrees/feat"), "");
        let folder = bare.substitute("/workspaces/${localWorkspaceFolderBasename}");
        assert_eq!(folder, "/workspaces/feat");

        let context = SubstitutionContext::new(Path::new("/home/dev/.am/worktrees/feat"), &folder);
        assert_eq!(context.substitute("${containerWorkspaceFolder}/bin"), "/workspaces/feat/bin");
        assert_eq!(context.substitute("${containerWorkspaceFolderBasename}"), "feat");
    }

    #[test]
    fn a_feature_entrypoint_is_substituted() {
        let resolved = ResolvedConfig {
            entrypoints: vec!["/init.sh --id ${devcontainerId}".to_string()],
            ..Default::default()
        };
        let json = parse_config_str(r#"{"image":"debian"}"#).unwrap();
        let context = ctx().with_devcontainer_id("am-feat-ab12cd");
        let finalized = finalize(resolved, &json, &context);
        assert_eq!(finalized.entrypoints, ["/init.sh --id am-feat-ab12cd"]);
    }

    #[test]
    fn env_is_an_alias_for_local_env() {
        std::env::set_var("AM_TEST_ALIAS", "yes");
        assert_eq!(ctx().substitute("${env:AM_TEST_ALIAS}"), "yes");
        std::env::remove_var("AM_TEST_ALIAS");
    }

    #[test]
    fn an_unset_environment_variable_uses_its_default() {
        std::env::remove_var("AM_TEST_UNSET");
        assert_eq!(ctx().substitute("${localEnv:AM_TEST_UNSET:fallback}"), "fallback");
        // Without a default it is the one case that legitimately expands to nothing.
        assert_eq!(ctx().substitute("${localEnv:AM_TEST_UNSET}"), "");
    }

    #[test]
    fn a_set_environment_variable_beats_its_default() {
        std::env::set_var("AM_TEST_SET", "real");
        assert_eq!(ctx().substitute("${localEnv:AM_TEST_SET:fallback}"), "real");
        std::env::remove_var("AM_TEST_SET");
    }

    #[test]
    fn container_env_resolves_against_the_containers_environment() {
        // The documented idiom is `"PATH": "${containerEnv:PATH}:/extra"`. Resolving it from
        // the config's own containerEnv — which almost never defines PATH — yielded `""`, so
        // the result *replaced* the image's PATH with `:/extra`.
        let mut context = ctx();
        context.container_env.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        assert_eq!(
            context.substitute("${containerEnv:PATH}:/extra"),
            "/usr/bin:/bin:/extra"
        );
        assert_eq!(context.substitute("${containerEnv:MISSING:none}"), "none");
    }

    #[test]
    fn devcontainer_id_expands_when_known_and_stays_literal_otherwise() {
        // A Feature naming a volume `dind-var-lib-docker-${devcontainerId}` must not end up
        // with the same name in every session on the host.
        assert_eq!(ctx().substitute("vol-${devcontainerId}"), "vol-${devcontainerId}");
        assert_eq!(
            ctx().with_devcontainer_id("am-feat-ab12cd").substitute("vol-${devcontainerId}"),
            "vol-am-feat-ab12cd"
        );
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

    #[test]
    fn both_mount_forms_accept_the_same_field_names() {
        // The object and string parsers are two spellings of one thing, so a name accepted by
        // one and rejected by the other is a bug waiting for a Feature author to find.
        let obj: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"mounts":[{"type":"volume","src":"v","dst":"/v"}]}]"#,
        )
        .unwrap();
        let string: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"mounts":["type=volume,src=v,dst=/v"]}]"#,
        )
        .unwrap();
        assert_eq!(merge(&obj).unwrap().mounts, merge(&string).unwrap().mounts);
    }

    #[test]
    fn app_port_publishes_the_way_the_cli_does() {
        // A bare port binds loopback; an explicit mapping passes through, since spelling it out
        // is how a config asks for something wider. Both read off the `docker run` line the
        // reference CLI builds.
        let json = parse_config_str(r#"{"image":"debian","appPort":[9000,"9100:9200","8080"]}"#)
            .unwrap();
        assert_eq!(
            json.app_port.publish_specs(),
            ["127.0.0.1:9000:9000", "9100:9200", "127.0.0.1:8080:8080"]
        );
    }

    #[test]
    fn app_port_accepts_a_bare_scalar() {
        let json = parse_config_str(r#"{"image":"debian","appPort":3000}"#).unwrap();
        assert_eq!(json.app_port.publish_specs(), ["127.0.0.1:3000:3000"]);
        let none = parse_config_str(r#"{"image":"debian"}"#).unwrap();
        assert!(none.app_port.publish_specs().is_empty());
    }

    #[test]
    fn a_parallel_hook_group_runs_its_members_concurrently() {
        // The object form exists for co-dependent commands: a server that never exits and a
        // waiter that depends on it. Sequential execution deadlocks on the first.
        let hook: LifecycleCommand = serde_json::from_str(
            r#"{"server":"npm start","wait":"wait-on http://localhost:3000"}"#,
        )
        .unwrap();
        let commands = hook.commands();
        assert_eq!(commands.len(), 1, "a group is one command, not a sequence");
        let script = commands[0].to_shell();
        assert!(script.contains("{ npm start ; } &"), "got: {script}");
        assert!(script.contains("{ wait-on http://localhost:3000 ; } &"), "got: {script}");
        // Every pid is waited on individually — a bare `wait` reports only the last, which
        // would let a failed member pass for success.
        assert!(script.contains("|| __am_rc=1"), "got: {script}");
    }

    #[test]
    fn a_single_member_group_needs_no_parallel_machinery() {
        let hook: LifecycleCommand = serde_json::from_str(r#"{"only":"echo hi"}"#).unwrap();
        assert_eq!(hook.commands(), vec![Command::Shell("echo hi".to_string())]);
    }

    #[test]
    fn workspace_mount_becomes_a_real_mount() {
        // Parsed, substituted, then dropped — so `workspaceFolder` pointed `--workdir` at a
        // path nothing was mounted at and the agent started in an empty directory.
        let resolved = ResolvedConfig {
            workspace_mount: Some(
                "source=/host/repo,target=/workspaces/app,type=bind".to_string(),
            ),
            ..Default::default()
        };
        // workspaceMount's source is the session worktree here, which is what makes it a
        // trusted mount under the default (non-opted-in) trust policy.
        let runtime = apply_trust(&resolved, &cfg_with(false), Path::new("/host/repo"));
        let found = runtime
            .mounts
            .iter()
            .find(|m| m.target == "/workspaces/app")
            .expect("workspaceMount must reach the run path");
        assert_eq!(found.source.as_deref(), Some("/host/repo"));
        assert_eq!(found.kind, "bind");
    }

    #[test]
    fn workspace_mount_does_not_displace_an_explicit_mount_on_the_same_target() {
        let resolved = ResolvedConfig {
            mounts: vec![Mount::Str(
                "source=/other,target=/workspaces/app,type=bind".to_string(),
            )
            .normalize()
            .unwrap()],
            workspace_mount: Some(
                "source=/host/repo,target=/workspaces/app,type=bind".to_string(),
            ),
            ..Default::default()
        };
        // "/other" is the surviving mount (the target collision means workspaceMount's own
        // push is skipped), so it must be the one the trust policy is judging.
        let runtime = apply_trust(&resolved, &cfg_with(false), Path::new("/other"));
        let same: Vec<_> =
            runtime.mounts.iter().filter(|m| m.target == "/workspaces/app").collect();
        assert_eq!(same.len(), 1, "two mounts on one target is a runtime error");
        assert_eq!(same[0].source.as_deref(), Some("/other"));
    }

    #[test]
    fn a_malformed_workspace_mount_is_a_warning_not_a_failure() {
        let resolved = ResolvedConfig {
            workspace_mount: Some("this is not a mount".to_string()),
            ..Default::default()
        };
        // The session is still usable via host-path mirroring, so this must not be fatal.
        let runtime = apply_trust(&resolved, &cfg_with(false), Path::new("/worktree"));
        assert!(runtime.mounts.is_empty());
    }

    #[test]
    fn update_remote_user_uid_defaults_to_true() {
        let runtime =
            apply_trust(&ResolvedConfig::default(), &cfg_with(false), Path::new("/worktree"));
        assert!(runtime.update_remote_user_uid, "the spec's default on Linux");

        let off = ResolvedConfig { update_remote_user_uid: Some(false), ..Default::default() };
        assert!(
            !apply_trust(&off, &cfg_with(false), Path::new("/worktree")).update_remote_user_uid
        );
    }

    #[test]
    fn an_object_mount_accepts_the_short_spellings() {
        // The image-metadata schema permits src/dst on the object form, and a Feature that
        // uses them would otherwise fail the whole label parse rather than one mount.
        let snippets: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"mounts":[{"type":"bind","src":"/host","dst":"/in"}]}]"#,
        )
        .unwrap();
        let merged = merge(&snippets).unwrap();
        assert_eq!(merged.mounts.len(), 1);
        assert_eq!(merged.mounts[0].source.as_deref(), Some("/host"));
        assert_eq!(merged.mounts[0].target, "/in");
    }

    #[test]
    fn build_options_and_cache_from_are_parsed() {
        let json = parse_config_str(
            r#"{"build":{"dockerfile":"Dockerfile","options":["--pull"],
                "cacheFrom":"ghcr.io/x/cache"}}"#,
        )
        .unwrap();
        let build = json.build.unwrap();
        assert_eq!(build.options, vec!["--pull"]);
        assert_eq!(build.cache_from.images(), vec!["ghcr.io/x/cache"]);
    }

    #[test]
    fn cache_from_accepts_an_array_too() {
        let json = parse_config_str(
            r#"{"build":{"dockerfile":"Dockerfile","cacheFrom":["a","b"]}}"#,
        )
        .unwrap();
        assert_eq!(json.build.unwrap().cache_from.images(), vec!["a", "b"]);
    }

    // ── Hashing ───────────────────────────────────────────────────────────────

    /// A repo with a Dockerfile that copies a file from its context.
    fn context_repo() -> tempfile::TempDir {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM debian\nCOPY app.txt /app.txt\n").unwrap();
        std::fs::write(
            dir.join("devcontainer.json"),
            r#"{"build":{"dockerfile":"Dockerfile","context":".."}}"#,
        )
        .unwrap();
        std::fs::write(tmp.path().join("app.txt"), "one").unwrap();
        for args in [
            vec!["init"],
            vec!["-c", "user.email=t@e.com", "-c", "user.name=T", "add", "-A"],
        ] {
            std::process::Command::new("git")
                .arg("-C")
                .arg(tmp.path())
                .args(args)
                .output()
                .unwrap();
        }
        tmp
    }

    #[test]
    fn editing_a_file_the_dockerfile_copies_changes_the_image_name() {
        // Otherwise `am start` reuses the image built from the old file and the edit appears to
        // do nothing — the most confusing way this can fail, because nothing reports an error.
        let tmp = context_repo();
        let config = tmp.path().join(".devcontainer/devcontainer.json");

        let before = config_hash(&config, &[]).unwrap();
        std::fs::write(tmp.path().join("app.txt"), "two").unwrap();
        assert_ne!(before, config_hash(&config, &[]).unwrap());
    }

    #[test]
    fn an_untracked_ignored_file_does_not_change_the_image_name() {
        // The bound that makes hashing a context affordable: `"context": ".."` is the whole
        // repository, and walking it unbounded would drag in target/ and node_modules/ — the
        // very paths a .dockerignore excludes and git already ignores.
        let tmp = context_repo();
        let config = tmp.path().join(".devcontainer/devcontainer.json");
        std::fs::write(tmp.path().join(".gitignore"), "junk/\n").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(tmp.path())
            .args(["add", "-A"])
            .output()
            .unwrap();

        let before = config_hash(&config, &[]).unwrap();
        std::fs::create_dir_all(tmp.path().join("junk")).unwrap();
        std::fs::write(tmp.path().join("junk/build.o"), "artifact").unwrap();
        assert_eq!(before, config_hash(&config, &[]).unwrap());
    }

    #[test]
    fn a_context_outside_a_repository_still_produces_a_hash() {
        // No git, no context contribution — the previous behaviour, and `--rebuild` remains the
        // answer. It must not fail the session.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Dockerfile"), "FROM debian\n").unwrap();
        let config = dir.join("devcontainer.json");
        std::fs::write(&config, r#"{"build":{"dockerfile":"Dockerfile"}}"#).unwrap();
        assert!(config_hash(&config, &[]).is_ok());
    }

    #[test]
    fn editing_a_vendored_feature_changes_the_image_name() {
        // Otherwise `am start` reuses the image built from the old install.sh and the edit
        // appears to do nothing — the same failure the Dockerfile is hashed to avoid.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(dir.join("vendored")).unwrap();
        let config = dir.join("devcontainer.json");
        std::fs::write(&config, r#"{"image":"debian","features":{"./vendored":{}}}"#).unwrap();
        std::fs::write(
            dir.join("vendored/devcontainer-feature.json"),
            r#"{"id":"vendored","version":"1.0.0"}"#,
        )
        .unwrap();
        let install = dir.join("vendored/install.sh");
        std::fs::write(&install, "#!/bin/sh\necho one\n").unwrap();

        let before = config_hash(&config, &[]).unwrap();
        std::fs::write(&install, "#!/bin/sh\necho two\n").unwrap();
        assert_ne!(before, config_hash(&config, &[]).unwrap());
    }

    #[test]
    fn adding_a_file_to_a_vendored_feature_changes_the_image_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(dir.join("vendored")).unwrap();
        let config = dir.join("devcontainer.json");
        std::fs::write(&config, r#"{"image":"debian","features":{"./vendored":{}}}"#).unwrap();
        std::fs::write(dir.join("vendored/install.sh"), "#!/bin/sh\n").unwrap();

        let before = config_hash(&config, &[]).unwrap();
        std::fs::write(dir.join("vendored/helper.sh"), "#!/bin/sh\n").unwrap();
        assert_ne!(before, config_hash(&config, &[]).unwrap(), "file names count too");
    }

    #[test]
    fn a_moved_lockfile_entry_changes_the_image_name() {
        // The whole point of hashing the lockfile: a registry Feature cannot be hashed by
        // content without a network round trip per `am start`, so the record of what it last
        // resolved to stands in for it. Move the pin, get a different image, rebuild.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("devcontainer.json");
        std::fs::write(
            &config,
            r#"{"image":"debian","features":{"ghcr.io/devcontainers/features/git:1":{}}}"#,
        )
        .unwrap();
        let lockfile = dir.join("devcontainer-lock.json");
        let write_lock = |digest: &str| {
            std::fs::write(
                &lockfile,
                format!(
                    r#"{{"features":{{"ghcr.io/devcontainers/features/git:1":{{
                        "version":"1.3.8",
                        "resolved":"ghcr.io/devcontainers/features/git@{digest}",
                        "integrity":"{digest}"}}}}}}"#
                ),
            )
            .unwrap();
        };

        write_lock("sha256:aaa");
        let pinned_old = config_hash(&config, &[]).unwrap();
        write_lock("sha256:bbb");
        let pinned_new = config_hash(&config, &[]).unwrap();
        assert_ne!(pinned_old, pinned_new, "a moved pin must produce a new image name");
    }

    #[test]
    fn adopting_a_lockfile_changes_the_image_name_once() {
        // Adding a lockfile to a repo that had none renames the image, so the next `am start`
        // rebuilds. That is a one-time cost and mostly a layer-cache hit; the alternative is
        // never noticing a moved tag at all.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("devcontainer.json");
        std::fs::write(&config, r#"{"image":"debian","features":{}}"#).unwrap();

        let unlocked = config_hash(&config, &[]).unwrap();
        std::fs::write(
            dir.join("devcontainer-lock.json"),
            r#"{"features":{"a":{"resolved":"a@sha256:1","integrity":"sha256:1"}}}"#,
        )
        .unwrap();
        assert_ne!(unlocked, config_hash(&config, &[]).unwrap());
    }

    #[test]
    fn a_registry_feature_does_not_make_the_hash_unstable() {
        // Only local Features are hashed by content. A registry one must not vary run to run,
        // since resolving it would mean a network round trip per `am start`.
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("devcontainer.json");
        std::fs::write(
            &config,
            r#"{"image":"debian","features":{"ghcr.io/devcontainers/features/git:1":{}}}"#,
        )
        .unwrap();
        assert_eq!(config_hash(&config, &[]).unwrap(), config_hash(&config, &[]).unwrap());
    }

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
    fn ports_attributes_can_ask_that_a_port_not_be_forwarded() {
        let snippets: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"portsAttributes":{"3000":{"label":"web","onAutoForward":"ignore"},
                                    "9000-9100":{"onAutoForward":"ignore"},
                                    "8080":{"onAutoForward":"notify"}}}]"#,
        )
        .unwrap();
        let merged = merge(&snippets).unwrap();

        assert!(merged.port_is_ignored(3000));
        // A range key covers every port in it, inclusive of both ends.
        assert!(merged.port_is_ignored(9000));
        assert!(merged.port_is_ignored(9050));
        assert!(merged.port_is_ignored(9100));
        assert!(!merged.port_is_ignored(9101));
        // Every other value of onAutoForward describes what an editor should do once the port
        // *is* forwarded, so it stays forwarded.
        assert!(!merged.port_is_ignored(8080));
        assert!(!merged.port_is_ignored(1234));
    }

    #[test]
    fn other_ports_attributes_covers_what_no_key_matches() {
        let snippets: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"portsAttributes":{"3000":{"onAutoForward":"notify"}},
                 "otherPortsAttributes":{"onAutoForward":"ignore"}}]"#,
        )
        .unwrap();
        let merged = merge(&snippets).unwrap();
        assert!(!merged.port_is_ignored(3000), "the keyed entry wins over the fallback");
        assert!(merged.port_is_ignored(8080));
    }

    #[test]
    fn ports_attributes_merge_key_by_key_across_snippets() {
        // A config describing one port must not discard what a Feature said about another.
        let snippets: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"portsAttributes":{"3000":{"onAutoForward":"ignore"}}},
                {"portsAttributes":{"8080":{"onAutoForward":"ignore"}}}]"#,
        )
        .unwrap();
        let merged = merge(&snippets).unwrap();
        assert!(merged.port_is_ignored(3000));
        assert!(merged.port_is_ignored(8080));
    }

    #[test]
    fn an_ignored_port_is_not_published() {
        let resolved = merge(&serde_json::from_str::<Vec<MetadataSnippet>>(
            r#"[{"forwardPorts":[3000,8080],
                 "portsAttributes":{"3000":{"onAutoForward":"ignore"}}}]"#,
        )
        .unwrap())
        .unwrap();
        let runtime =
            apply_trust(&resolved, &crate::config::Config::default(), Path::new("/worktree"));
        assert_eq!(runtime.ports, vec![ForwardedPort::Own(8080)]);
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
        let runtime = apply_trust(&escalating(), &cfg_with(false), Path::new("/worktree"));
        assert!(!runtime.privileged);
        assert!(runtime.cap_add.is_empty());
        assert!(runtime.security_opt.is_empty());
        assert!(runtime.run_args.is_empty());
    }

    #[test]
    fn trust_gate_grants_escalating_options_when_opted_in() {
        let runtime = apply_trust(&escalating(), &cfg_with(true), Path::new("/worktree"));
        assert!(runtime.privileged);
        assert_eq!(runtime.cap_add, vec!["SYS_ADMIN".to_string()]);
        assert_eq!(runtime.security_opt, vec!["label=disable".to_string()]);
        assert_eq!(runtime.run_args, vec!["--network=host".to_string()]);
    }

    #[test]
    fn trust_gate_keeps_non_escalating_settings_either_way() {
        // init comes from ordinary Features (sshd, docker-outside-of-docker) and does not
        // hand the container new authority over the host.
        let runtime = apply_trust(&escalating(), &cfg_with(false), Path::new("/worktree"));
        assert!(runtime.init);
    }

    #[test]
    fn trust_gate_merges_container_and_remote_env() {
        let mut resolved = ResolvedConfig::default();
        resolved
            .container_env
            .insert("A".to_string(), "1".to_string());
        resolved.remote_env.insert("B".to_string(), Some("2".to_string()));
        let runtime = apply_trust(&resolved, &cfg_with(false), Path::new("/worktree"));
        assert!(runtime.env.contains(&("A".to_string(), "1".to_string())));
        assert!(runtime.env.contains(&("B".to_string(), "2".to_string())));
    }

    // ── Mount trust ──────────────────────────────────────────────────────────

    fn bind(source: &str, target: &str) -> NormalizedMount {
        NormalizedMount {
            source: Some(source.to_string()),
            target: target.to_string(),
            kind: "bind".to_string(),
            read_only: false,
        }
    }

    #[test]
    fn mount_trust_drops_a_bind_of_the_host_root() {
        let resolved = ResolvedConfig { mounts: vec![bind("/", "/host")], ..Default::default() };
        let runtime = apply_trust(&resolved, &cfg_with(false), Path::new("/worktree"));
        assert!(runtime.mounts.is_empty(), "must not mount the host root: {:?}", runtime.mounts);
    }

    #[test]
    fn mount_trust_drops_a_path_under_home_outside_the_worktree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();

        let resolved = ResolvedConfig {
            mounts: vec![bind(&home.to_string_lossy(), "/host-home")],
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), &worktree);
        assert!(runtime.mounts.is_empty());
    }

    #[test]
    fn mount_trust_drops_a_sibling_repository_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("repo-a");
        std::fs::create_dir_all(&worktree).unwrap();
        let sibling = tmp.path().join("repo-b");
        std::fs::create_dir_all(&sibling).unwrap();

        let resolved = ResolvedConfig {
            mounts: vec![bind(&sibling.to_string_lossy(), "/other-repo")],
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), &worktree);
        assert!(runtime.mounts.is_empty());
    }

    #[test]
    fn mount_trust_drops_a_symlink_that_escapes_the_worktree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // A symlink living *inside* the worktree, but resolving outside it — the point of
        // canonicalizing before deciding rather than trusting the literal path.
        let link = worktree.join("escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let resolved = ResolvedConfig {
            mounts: vec![bind(&link.to_string_lossy(), "/escape")],
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), &worktree);
        assert!(runtime.mounts.is_empty(), "a symlink must not launder a path out of the worktree");
    }

    #[test]
    fn mount_trust_drops_a_dotdot_traversal_out_of_the_worktree() {
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(tmp.path().join("secret")).unwrap();
        let traversal = worktree.join("../secret");

        let resolved = ResolvedConfig {
            mounts: vec![bind(&traversal.to_string_lossy(), "/secret")],
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), &worktree);
        assert!(runtime.mounts.is_empty());
    }

    #[test]
    fn mount_trust_allows_a_worktree_internal_bind() {
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("worktree");
        let sub = worktree.join("data");
        std::fs::create_dir_all(&sub).unwrap();

        let resolved = ResolvedConfig {
            mounts: vec![bind(&sub.to_string_lossy(), "/data")],
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), &worktree);
        assert_eq!(runtime.mounts.len(), 1);
        assert_eq!(runtime.mounts[0].target, "/data");
    }

    #[test]
    fn mount_trust_allows_a_worktree_internal_bind_that_does_not_exist_yet() {
        // The runtime creates a missing bind source on start, so a not-yet-existing
        // subdirectory under the worktree is exactly as legitimate as an existing one — the
        // reason `resolve_for_trust` walks up to the nearest existing ancestor instead of
        // treating "does not exist" as automatically untrusted.
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let not_yet = worktree.join("build-cache");

        let resolved = ResolvedConfig {
            mounts: vec![bind(&not_yet.to_string_lossy(), "/cache")],
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), &worktree);
        assert_eq!(runtime.mounts.len(), 1);
    }

    #[test]
    fn mount_trust_always_allows_named_volumes_and_tmpfs() {
        // Neither names a host path, so neither is something the trust policy needs to judge.
        let resolved = ResolvedConfig {
            mounts: vec![
                NormalizedMount {
                    source: Some("my-volume".to_string()),
                    target: "/data".to_string(),
                    kind: "volume".to_string(),
                    read_only: false,
                },
                NormalizedMount {
                    source: None,
                    target: "/tmp/scratch".to_string(),
                    kind: "tmpfs".to_string(),
                    read_only: false,
                },
            ],
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), Path::new("/worktree"));
        assert_eq!(runtime.mounts.len(), 2);
    }

    #[test]
    fn mount_trust_honours_every_bind_when_opted_in() {
        let resolved = ResolvedConfig { mounts: vec![bind("/", "/host")], ..Default::default() };
        let runtime = apply_trust(&resolved, &cfg_with(true), Path::new("/worktree"));
        assert_eq!(runtime.mounts.len(), 1);
        assert_eq!(runtime.mounts[0].source.as_deref(), Some("/"));
    }

    #[test]
    fn mount_trust_drops_a_workspace_mount_pointed_outside_the_worktree() {
        // workspaceMount is folded into the same list the other mounts go through, so it must
        // be judged by the same policy rather than being a way around it.
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();

        let resolved = ResolvedConfig {
            workspace_mount: Some(format!(
                "source={},target=/workspaces/app,type=bind",
                elsewhere.to_string_lossy()
            )),
            ..Default::default()
        };
        let runtime = apply_trust(&resolved, &cfg_with(false), &worktree);
        assert!(runtime.mounts.is_empty());
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
    fn post_attach_is_kept_out_of_the_startup_hooks() {
        // Not because it never runs — it does, both chained after these and `exec`'d into an
        // already-running container by `am attach`. It is separate because it is the one hook
        // that is not once-per-container, so recording it in `lifecycle_done` alongside the
        // create-time hooks would be a lie.
        let (snippets, names) = startup_commands(&with_hooks(), false);
        assert!(!snippets.iter().any(|s| s.contains("attach")));
        assert!(!names.iter().any(|n| n == "postAttachCommand"));
    }

    #[test]
    fn attach_commands_are_the_post_attach_hook() {
        assert_eq!(attach_commands(&with_hooks(), false), vec!["echo attach"]);
    }

    #[test]
    fn skip_lifecycle_suppresses_the_attach_hook_too() {
        assert!(attach_commands(&with_hooks(), true).is_empty());
    }

    #[test]
    fn a_config_with_no_post_attach_has_nothing_to_run() {
        // The common case, and the one that decides whether `am attach` execs into the
        // container at all.
        assert!(attach_commands(&ResolvedConfig::default(), false).is_empty());
    }

    #[test]
    fn every_post_attach_contributor_runs_in_order() {
        // Features and the config can each contribute one; the merge keeps them all.
        let snippets: Vec<MetadataSnippet> = serde_json::from_str(
            r#"[{"postAttachCommand":"echo from-feature"},{"postAttachCommand":["echo","from-config"]}]"#,
        )
        .unwrap();
        let merged = merge(&snippets).unwrap();
        assert_eq!(
            attach_commands(&merged, false),
            vec!["echo from-feature", "echo from-config"]
        );
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

