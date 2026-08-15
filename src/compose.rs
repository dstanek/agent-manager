//! Compose-backed devcontainer sessions.
//!
//! A `dockerComposeFile` config is not one container, so it cannot go through
//! [`crate::container::build_run_command`]. The environment is a whole compose project — the
//! agent's service plus whatever it depends on — and `am` has to bring the project up, run the
//! agent *inside* the named service, and take the project down again on destroy.
//!
//! The design keeps `am` authoritative over the same things it owns for a single container:
//!
//! ```text
//! am start   ─► build the service's image, Features baked in   (the ordinary builder)
//!            ─► write an override pinning that image and adding am's mounts and env
//!            ─► compose -p am-<slug> up -d
//!            ─► compose -p am-<slug> exec <service> <agent>
//! am destroy ─► compose -p am-<slug> down -v
//! ```
//!
//! **`am` never parses YAML.** The compose file belongs to the project and can use anchors,
//! extends, interpolation and profiles; re-implementing that would be a second source of truth.
//! Instead the runtime is asked for the resolved model (`compose config --format json`), and the
//! override `am` contributes is *written* as JSON — which compose accepts, because JSON is
//! valid YAML. That buys correct quoting for paths and env values for free.
//!
//! The override is a separate file layered last rather than an edit of the project's own, so
//! nothing `am` does can corrupt a file the repo owns.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::command::{run_built_command, run_built_command_output};
use crate::config::NetworkMode;
use crate::container::{
    self, ContainerMounts, ContainerRuntime, DevcontainerRuntime, MountMode,
};
use crate::devcontainer::ForwardedPort;
use crate::error::AmError;

/// Turn an argv into a `Command`. Compose invocations are assembled as `Vec<String>` so they can
/// be asserted on in tests without running anything.
fn command(argv: &[String]) -> std::process::Command {
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd
}

/// Everything needed to address a session's compose project again later.
///
/// Persisted in the session record because `am destroy` and `am attach` must reach the project
/// without re-reading a `devcontainer.json` that may have changed underneath them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionCompose {
    pub project: String,
    pub service: String,
    /// The project's own compose files, in the order the config listed them.
    pub files: Vec<PathBuf>,
    /// The override `am` generated, layered after `files`.
    pub override_path: PathBuf,
}

/// The compose project name for a session.
///
/// Compose restricts these to lowercase alphanumerics, `-` and `_`, which `am`'s slug validation
/// already guarantees; the sanitising here is belt-and-braces for a slug that reaches this by
/// another route. The `am-` prefix makes a session's project recognisable in `docker compose ls`,
/// and it is unique because the session store is global per user — two checkouts of the same repo
/// cannot both hold the slug.
pub fn project_name(slug: &str) -> String {
    let sanitized: String = slug
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    format!("am-{}", sanitized.to_lowercase())
}

/// `compose` subcommand args for a runtime.
///
/// Both `docker compose` and `podman compose` take the same shape; the older standalone
/// `docker-compose` binary is not looked for, since every supported runtime ships the plugin.
fn compose_args(runtime_bin: &Path, files: &[PathBuf], project: &str) -> Vec<String> {
    let mut args = vec![runtime_bin.to_string_lossy().into_owned(), "compose".to_string()];
    for file in files {
        args.push("-f".to_string());
        args.push(file.to_string_lossy().into_owned());
    }
    args.push("-p".to_string());
    args.push(project.to_string());
    args
}

/// Every compose file for the project, the repo's own first and `am`'s override last.
fn all_files(compose: &SessionCompose) -> Vec<PathBuf> {
    let mut files = compose.files.clone();
    files.push(compose.override_path.clone());
    files
}

/// Ask the runtime to resolve the compose files into a single normalised model.
///
/// This is what keeps a YAML parser out of `am`: interpolation, `extends`, anchors and merge
/// keys are all applied by the tool that owns the format.
pub fn resolved_config(runtime_bin: &Path, files: &[PathBuf]) -> Result<Value> {
    let mut args = compose_args(runtime_bin, files, "am-config-probe");
    args.push("config".to_string());
    args.push("--format".to_string());
    args.push("json".to_string());
    let out = run_built_command_output(command(&args), AmError::ContainerError)
        .with_context(|| "resolving the compose file".to_string())?;
    serde_json::from_str(&out).with_context(|| "parsing the resolved compose config".to_string())
}

/// The service `am` runs the agent in, and the image or build it is defined with.
#[derive(Debug)]
pub struct ServiceDefinition {
    pub image: Option<String>,
    /// `build.context` and `build.dockerfile`, already absolute — compose resolves them
    /// relative to the file that declared them, and reports them that way.
    pub build: Option<(PathBuf, Option<PathBuf>)>,
}

/// Read one service out of a resolved compose model.
pub fn service_definition(config: &Value, service: &str) -> Result<ServiceDefinition> {
    let svc = config
        .get("services")
        .and_then(|s| s.get(service))
        .ok_or_else(|| {
            let known = config
                .get("services")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            AmError::ConfigError(format!(
                "the devcontainer names service '{service}', which the compose file does not \
                 define (it defines: {known})"
            ))
        })?;

    let image = svc.get("image").and_then(Value::as_str).map(str::to_string);
    let build = svc.get("build").and_then(Value::as_object).map(|b| {
        let context = b
            .get("context")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dockerfile = b.get("dockerfile").and_then(Value::as_str).map(PathBuf::from);
        (context, dockerfile)
    });

    if image.is_none() && build.is_none() {
        return Err(AmError::ConfigError(format!(
            "compose service '{service}' has neither an image nor a build section"
        ))
        .into());
    }
    Ok(ServiceDefinition { image, build })
}

/// Build the override document that carries `am`'s own contribution to the service.
///
/// This is the compose counterpart of [`crate::container::build_run_command`], and it is
/// deliberately the *only* place the two paths differ in what a session gets. Anything added
/// there has to be added here or a compose session quietly loses it.
///
/// Two things are deliberately **not** set. The service's `command` is left alone, because the
/// devcontainer spec defaults `overrideCommand` to false for compose — the project's own file is
/// responsible for keeping the service alive, which is why the convention is `sleep infinity`.
/// And nothing is written for other services: `am` contributes to the one it runs the agent in
/// and leaves the rest of the project exactly as the repo described it.
#[allow(clippy::too_many_arguments)]
pub fn override_document(
    runtime: &ContainerRuntime,
    service: &str,
    image: &str,
    mounts: &ContainerMounts,
    env_passthrough: &[String],
    extra_env: &[(String, String)],
    dc: &DevcontainerRuntime,
) -> Value {
    let home = &mounts.container_home;
    let selinux = container::use_selinux_labels(runtime);
    let mut volumes: Vec<String> = Vec::new();
    macro_rules! push_mount {
        ($host:expr, $target:expr, $mode:expr, $relabel:expr) => {
            volumes.push(container::mount_str($host, $target, $mode, $relabel))
        };
    }

    // The worktree and the VCS dir at their host paths — the mirroring that makes both git
    // worktrees and jj workspaces work, and the reason this is worth owning rather than
    // handing to a tool that insists on mounting the workspace its own way.
    push_mount!(
        &mounts.worktree_host,
        &mounts.worktree_host.to_string_lossy(),
        MountMode::ReadWrite,
        selinux
    );
    push_mount!(
        &mounts.vcs_host,
        &mounts.vcs_host.to_string_lossy(),
        MountMode::ReadWrite,
        selinux
    );
    if let Some(git) = &mounts.colocated_git_host {
        push_mount!(git, &git.to_string_lossy(), MountMode::ReadWrite, selinux);
    }
    if mounts.gitconfig_host.exists() {
        push_mount!(
            &mounts.gitconfig_host,
            &format!("{home}/.gitconfig"),
            MountMode::ReadOnly,
            selinux
        );
    }
    if mounts.ssh_host.exists() {
        push_mount!(&mounts.ssh_host, &format!("{home}/.ssh"), MountMode::ReadOnly, selinux);
    }

    let mut environment: BTreeMap<String, String> = BTreeMap::new();

    // The agent socket is a host-owned file: bind it at its own path so SSH_AUTH_SOCK carries
    // over unchanged, and never relabel it — `:z` would rewrite a label the host still needs.
    if let Some(sock) = &mounts.ssh_agent_sock {
        if sock.exists() {
            push_mount!(sock, &sock.to_string_lossy(), MountMode::ReadWrite, false);
            environment.insert(
                "SSH_AUTH_SOCK".to_string(),
                sock.to_string_lossy().into_owned(),
            );
        }
    }

    for auth in &mounts.agent_auth {
        if auth.host_path.exists() {
            push_mount!(
                &auth.host_path,
                &auth.container_path.to_string_lossy(),
                auth.mode.clone(),
                selinux
            );
        }
    }

    for mount in &dc.mounts {
        let Some(source) = &mount.source else { continue };
        let mode = if mount.read_only { MountMode::ReadOnly } else { MountMode::ReadWrite };
        if mount.kind == "bind" {
            push_mount!(Path::new(source), &mount.target, mode, selinux);
        } else {
            let mode_str = if mount.read_only { "ro" } else { "rw" };
            volumes.push(format!("{source}:{}:{mode_str}", mount.target));
        }
    }

    // Same precedence as the run path: jj identity first so anything explicit still wins.
    for (key, val) in container::jj_identity_env(&mounts.gitconfig_host) {
        environment.insert(key, val);
    }
    for (key, val) in extra_env {
        environment.insert(key.clone(), val.clone());
    }
    for (key, val) in &dc.env {
        environment.insert(key.clone(), val.clone());
    }

    let mut service_doc = Map::new();
    service_doc.insert("image".to_string(), json!(image));
    service_doc.insert("volumes".to_string(), json!(volumes));
    service_doc.insert("environment".to_string(), json!(environment));

    // A host variable with no value passes through by name; compose spells that as a null.
    if !env_passthrough.is_empty() {
        let mut passthrough = Map::new();
        for var in env_passthrough {
            match var.split_once('=') {
                Some((k, v)) => passthrough.insert(k.to_string(), json!(v)),
                None => passthrough.insert(var.clone(), Value::Null),
            };
        }
        // Merge under the explicit values, which were resolved by am and must win.
        if let Some(Value::Object(existing)) = service_doc.get("environment") {
            for (k, v) in existing {
                passthrough.insert(k.clone(), v.clone());
            }
        }
        service_doc.insert("environment".to_string(), Value::Object(passthrough));
    }

    if let Some(user) = &dc.user {
        service_doc.insert("user".to_string(), json!(user));
    } else if let Some((uid, gid)) = container::get_host_uid_gid() {
        // Matching the run path: without a named devcontainer user, run as the host's own
        // uid/gid so bind-mounted files stay writable.
        service_doc.insert("user".to_string(), json!(format!("{uid}:{gid}")));
    }

    if dc.init {
        service_doc.insert("init".to_string(), json!(true));
    }
    if dc.privileged {
        service_doc.insert("privileged".to_string(), json!(true));
    }
    if !dc.cap_add.is_empty() {
        service_doc.insert("cap_add".to_string(), json!(dc.cap_add));
    }
    if !dc.security_opt.is_empty() {
        service_doc.insert("security_opt".to_string(), json!(dc.security_opt));
    }
    service_doc.insert(
        "working_dir".to_string(),
        json!(match &dc.workdir {
            Some(dir) => dir.clone(),
            None => mounts.worktree_host.to_string_lossy().into_owned(),
        }),
    );

    // forwardPorts. A bare port belongs to the agent's own service; a `"<service>:<port>"` entry
    // names another one, which is the one case where `am` writes an override for a service it
    // does not run the agent in — publishing what the config asked for is the whole point, and
    // refusing would make the entry silently inert.
    let mut services = Map::new();
    let mut own_ports: Vec<String> = Vec::new();
    for port in &dc.ports {
        match port {
            ForwardedPort::Own(p) => own_ports.push(ForwardedPort::publish_spec(*p)),
            ForwardedPort::Service { service: other, port } => {
                let entry = services
                    .entry(other.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                if let Some(obj) = entry.as_object_mut() {
                    obj.entry("ports")
                        .or_insert_with(|| Value::Array(Vec::new()))
                        .as_array_mut()
                        .expect("ports is an array")
                        .push(json!(ForwardedPort::publish_spec(*port)));
                }
            }
        }
    }
    if !own_ports.is_empty() {
        service_doc.insert("ports".to_string(), json!(own_ports));
    }

    services.insert(service.to_string(), Value::Object(service_doc));
    json!({ "services": services })
}

/// `container.network = "none"` cannot be honoured for a compose project.
///
/// Compose services reach each other over a project network, so switching the agent's service to
/// `network_mode: none` would cut it off from the very services the config exists to provide.
/// Silently ignoring the setting is worse than refusing: it is a security control, and a user who
/// set it is entitled to know it did not apply.
pub fn check_network(network: &NetworkMode) -> Result<()> {
    if matches!(network, NetworkMode::None) {
        return Err(AmError::ConfigError(
            "container.network = \"none\" cannot be applied to a compose devcontainer, because \
             its services reach each other over the compose network\n\
             Leave container.network at its default, or use container.mode = \"image\" for an \
             isolated single-container session"
                .to_string(),
        )
        .into());
    }
    Ok(())
}

/// Write the override beside the session's other state and return its path.
pub fn write_override(dir: &Path, document: &Value) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    // `.yml` rather than `.json` because compose only recognises the extension for its own
    // discovery, and a reader opening this file should see something compose would accept.
    let path = dir.join("compose-override.yml");
    let body = serde_json::to_string_pretty(document)
        .with_context(|| "rendering the compose override".to_string())?;
    std::fs::write(&path, format!("{body}\n"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Bring the project up in the background.
pub fn up(runtime_bin: &Path, compose: &SessionCompose) -> Result<()> {
    let mut args = compose_args(runtime_bin, &all_files(compose), &compose.project);
    args.extend(["up".to_string(), "-d".to_string()]);
    run_built_command(command(&args), AmError::ContainerError)
        .with_context(|| "starting the compose project")
}

/// Take the project down, removing its volumes.
///
/// `-v` matters: a session's environment is meant to be disposable, and leaving anonymous
/// volumes behind would accumulate state across `am destroy`/`am start` cycles that the user
/// never asked to keep.
pub fn down(runtime_bin: &Path, compose: &SessionCompose) -> Result<()> {
    let mut args = compose_args(runtime_bin, &all_files(compose), &compose.project);
    args.extend(["down".to_string(), "-v".to_string()]);
    run_built_command(command(&args), AmError::ContainerError)
        .with_context(|| "stopping the compose project")
}

/// The command that runs the agent inside the service, for the tmux pane.
pub fn exec_command(
    runtime_bin: &Path,
    compose: &SessionCompose,
    agent_cmd: &[String],
) -> Vec<String> {
    let mut args = compose_args(runtime_bin, &all_files(compose), &compose.project);
    args.push("exec".to_string());
    args.push(compose.service.clone());
    args.extend(agent_cmd.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::RuntimeKind;

    fn runtime() -> ContainerRuntime {
        ContainerRuntime { kind: RuntimeKind::Docker, bin: PathBuf::from("/usr/bin/docker") }
    }

    fn compose() -> SessionCompose {
        SessionCompose {
            project: "am-feat".to_string(),
            service: "app".to_string(),
            files: vec![PathBuf::from("/repo/.devcontainer/docker-compose.yml")],
            override_path: PathBuf::from("/state/compose-override.yml"),
        }
    }

    #[test]
    fn project_names_are_prefixed_and_compose_safe() {
        assert_eq!(project_name("my-feature"), "am-my-feature");
        // Compose only accepts lowercase alphanumerics, dashes and underscores.
        assert_eq!(project_name("Feat/One"), "am-feat-one");
    }

    #[test]
    fn the_override_is_layered_last() {
        // The repo's own files must come first, so am's contribution wins on conflict.
        let args = compose_args(Path::new("/usr/bin/docker"), &all_files(&compose()), "am-feat");
        let joined = args.join(" ");
        let repo = joined.find("docker-compose.yml").unwrap();
        let ours = joined.find("compose-override.yml").unwrap();
        assert!(repo < ours, "am's override must be the last -f");
        assert!(joined.contains("-p am-feat"));
    }

    #[test]
    fn a_missing_service_names_what_the_compose_file_does_define() {
        let config = json!({"services": {"web": {"image": "nginx"}, "db": {"image": "postgres"}}});
        let err = service_definition(&config, "app").unwrap_err().to_string();
        assert!(err.contains("app"), "must name the service that is missing: {err}");
        assert!(err.contains("web") && err.contains("db"), "must list what exists: {err}");
    }

    #[test]
    fn a_service_with_neither_image_nor_build_is_an_error() {
        let config = json!({"services": {"app": {"command": "sleep infinity"}}});
        let err = service_definition(&config, "app").unwrap_err().to_string();
        assert!(err.contains("neither an image nor a build"), "got: {err}");
    }

    #[test]
    fn a_built_service_reports_its_context_and_dockerfile() {
        let config = json!({"services": {"app": {
            "build": {"context": "/repo", "dockerfile": "Dockerfile.dev"}
        }}});
        let def = service_definition(&config, "app").unwrap();
        assert!(def.image.is_none());
        let (context, dockerfile) = def.build.unwrap();
        assert_eq!(context, PathBuf::from("/repo"));
        assert_eq!(dockerfile, Some(PathBuf::from("Dockerfile.dev")));
    }

    #[test]
    fn network_none_is_refused_rather_than_ignored() {
        let err = check_network(&NetworkMode::None).unwrap_err().to_string();
        assert!(err.contains("compose network"), "must explain why: {err}");
        assert!(err.contains("container.mode"), "must offer a way forward: {err}");
        assert!(check_network(&NetworkMode::Full).is_ok());
    }

    #[test]
    fn the_override_is_valid_json_so_compose_can_read_it_as_yaml() {
        let doc = json!({"services": {"app": {"image": "am-dc-abc"}}});
        let tmp = tempfile::tempdir().unwrap();
        let path = write_override(tmp.path(), &doc).unwrap();
        assert_eq!(path.file_name().unwrap(), "compose-override.yml");
        let text = std::fs::read_to_string(&path).unwrap();
        // JSON is a subset of YAML, which is what lets am emit this without a YAML writer —
        // and what gives it correct quoting for paths and env values for free.
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, doc);
    }

    /// The override `am` generates has to satisfy compose's own schema.
    ///
    /// Every other test here asserts on the document `am` builds, which cannot catch a key
    /// spelled the way `docker run` spells it rather than the way compose does — `working_dir`
    /// vs `--workdir`, `cap_add` vs `--cap-add`, environment as a map rather than a list. Only
    /// the real tool can say. Ignored by default because it needs a container runtime.
    #[test]
    #[ignore = "needs a container runtime"]
    fn the_generated_override_satisfies_composes_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("docker-compose.yml");
        std::fs::write(
            &base,
            "services:\n  app:\n    image: debian:bookworm-slim\n    command: sleep infinity\n",
        )
        .unwrap();

        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let gitconfig = home.join(".gitconfig");
        std::fs::write(&gitconfig, "[user]\n\tname = Test\n\temail = t@example.com\n").unwrap();

        let mounts = ContainerMounts {
            worktree_host: tmp.path().join("worktree"),
            vcs_host: tmp.path().join("worktree/.git"),
            colocated_git_host: None,
            gitconfig_host: gitconfig,
            ssh_host: home.join(".ssh"),
            ssh_agent_sock: None,
            agent_auth: Vec::new(),
            container_home: "/home/vscode".to_string(),
        };
        let mut dc = DevcontainerRuntime::default();
        dc.env.push(("A_VAR".to_string(), "a value with spaces".to_string()));
        dc.cap_add.push("SYS_PTRACE".to_string());
        dc.init = true;
        dc.user = Some("vscode".to_string());

        let doc = override_document(
            &runtime(),
            "app",
            "am-dc-test",
            &mounts,
            &["HOME".to_string(), "LANG=C".to_string()],
            &[("TOKEN".to_string(), "secret".to_string())],
            &dc,
        );
        let override_path = write_override(tmp.path(), &doc).unwrap();

        let out = std::process::Command::new("docker")
            .args(["compose", "-f"])
            .arg(&base)
            .arg("-f")
            .arg(&override_path)
            .args(["config", "--format", "json"])
            .output()
            .expect("running docker compose");
        assert!(
            out.status.success(),
            "compose rejected the override: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let resolved: Value = serde_json::from_slice(&out.stdout).unwrap();
        let app = &resolved["services"]["app"];
        assert_eq!(app["image"], json!("am-dc-test"));
        assert_eq!(app["user"], json!("vscode"));
        assert_eq!(app["init"], json!(true));
        assert_eq!(app["cap_add"], json!(["SYS_PTRACE"]));
        assert_eq!(app["environment"]["A_VAR"], json!("a value with spaces"));
        assert_eq!(app["environment"]["TOKEN"], json!("secret"));
        // The compose file's own command must survive: it is what keeps the service alive.
        assert_eq!(app["command"], json!(["sleep", "infinity"]));
    }

    #[test]
    fn forwarded_ports_are_published_on_the_right_service() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts = ContainerMounts {
            worktree_host: tmp.path().to_path_buf(),
            vcs_host: tmp.path().join(".git"),
            colocated_git_host: None,
            gitconfig_host: tmp.path().join("gitconfig"),
            ssh_host: tmp.path().join("ssh"),
            ssh_agent_sock: None,
            agent_auth: Vec::new(),
            container_home: "/home/vscode".to_string(),
        };
        let dc = DevcontainerRuntime {
            ports: vec![
                ForwardedPort::Own(3000),
                ForwardedPort::Service { service: "db".into(), port: 5432 },
            ],
            ..DevcontainerRuntime::default()
        };
        let doc = override_document(&runtime(), "app", "img", &mounts, &[], &[], &dc);
        let services = doc["services"].as_object().unwrap();

        assert_eq!(services["app"]["ports"], json!(["127.0.0.1:3000:3000"]));
        // The one case where am writes an override for a service it does not run the agent in:
        // a `"db:5432"` entry names another service, and publishing it is the whole point.
        assert_eq!(services["db"]["ports"], json!(["127.0.0.1:5432:5432"]));
        // ...and it contributes nothing else to that service.
        assert_eq!(services["db"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn a_service_with_no_forwarded_ports_gets_no_ports_key() {
        let tmp = tempfile::tempdir().unwrap();
        let mounts = ContainerMounts {
            worktree_host: tmp.path().to_path_buf(),
            vcs_host: tmp.path().join(".git"),
            colocated_git_host: None,
            gitconfig_host: tmp.path().join("gitconfig"),
            ssh_host: tmp.path().join("ssh"),
            ssh_agent_sock: None,
            agent_auth: Vec::new(),
            container_home: "/home/vscode".to_string(),
        };
        let doc = override_document(
            &runtime(),
            "app",
            "img",
            &mounts,
            &[],
            &[],
            &DevcontainerRuntime::default(),
        );
        assert!(doc["services"]["app"].as_object().unwrap().get("ports").is_none());
    }

    #[test]
    fn exec_runs_the_agent_in_the_named_service() {
        let cmd = exec_command(
            Path::new("/usr/bin/docker"),
            &compose(),
            &["claude".to_string(), "--continue".to_string()],
        );
        let joined = cmd.join(" ");
        assert!(joined.contains("compose"));
        assert!(joined.ends_with("exec app claude --continue"), "got: {joined}");
    }
}
