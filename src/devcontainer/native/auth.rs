//! Registry credentials, read from the files the container runtimes already keep.
//!
//! A Feature in a private registry answers `401` to an anonymous pull. `am` does not ask for
//! credentials or store any of its own: it reads the same files `docker login` and `podman
//! login` write, so a user who can already pull the image can already pull the Feature.
//!
//! Both runtimes matter here and they do not agree on a location:
//!
//! ```text
//! $REGISTRY_AUTH_FILE                     podman, explicit override
//! $DOCKER_CONFIG/config.json              docker, explicit override
//! ~/.docker/config.json                   docker, default
//! $XDG_RUNTIME_DIR/containers/auth.json   podman, default
//! ~/.config/containers/auth.json          podman, persistent
//! ```
//!
//! The file format is the same either way: an `auths` map, optionally with credential helpers
//! delegating to an external binary. Helpers are how a login survives on a machine with a
//! keychain, so skipping them would mean "works until you use Docker Desktop".
//!
//! Nothing here is logged. A credential that reaches an error message is a credential in a
//! terminal scrollback and a CI log.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// A username and secret for one registry.
///
/// `secret` is a password or a token; the distinction does not matter to the callers, which
/// only ever put it in an `Authorization` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub secret: String,
}

impl Credentials {
    /// The value for an HTTP `Basic` authorization header.
    pub fn basic_header(&self) -> String {
        use base64::Engine;
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.username, self.secret));
        format!("Basic {encoded}")
    }
}

#[derive(Debug, Default, Deserialize)]
struct AuthFile {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
    /// A helper used for every registry without a more specific one.
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
    /// Per-registry helpers, which win over `credsStore`.
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthEntry {
    /// base64 of `username:secret`, which is how `docker login` writes it.
    #[serde(default)]
    auth: Option<String>,
    /// Some tools write these directly instead.
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

/// Credentials for a registry host, or `None` for an anonymous pull.
///
/// Memoised per host for the life of the process. A build resolves one manifest per Feature and
/// they usually share a registry, so without this a credential helper would be invoked once per
/// Feature — and some helpers front a keychain that prompts, which would mean a stack of
/// dialogs for a single `am start`.
pub fn for_registry(registry: &str) -> Option<Credentials> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Credentials>>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = normalize_host(registry);
    if let Ok(map) = cache.lock() {
        if let Some(found) = map.get(&key) {
            return found.clone();
        }
    }
    let found = look_up(registry);
    if let Ok(mut map) = cache.lock() {
        map.insert(key, found.clone());
    }
    found
}

fn look_up(registry: &str) -> Option<Credentials> {
    for path in auth_file_paths() {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let Ok(file) = serde_json::from_str::<AuthFile>(&text) else { continue };

        // A per-registry helper is the most specific answer, so it is asked first.
        if let Some(helper) = lookup(&file.cred_helpers, registry) {
            if let Some(found) = from_helper(helper, registry) {
                return Some(found);
            }
        }
        if let Some(entry) = lookup(&file.auths, registry) {
            if let Some(found) = from_entry(entry) {
                return Some(found);
            }
        }
        if let Some(store) = &file.creds_store {
            if let Some(found) = from_helper(store, registry) {
                return Some(found);
            }
        }
    }
    None
}

/// The files to consult, in order of specificity. Missing ones are skipped.
fn auth_file_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(explicit) = std::env::var("REGISTRY_AUTH_FILE") {
        paths.push(PathBuf::from(explicit));
    }
    if let Ok(dir) = std::env::var("DOCKER_CONFIG") {
        paths.push(PathBuf::from(dir).join("config.json"));
    }
    if let Ok(home) = std::env::var("HOME") {
        paths.push(PathBuf::from(&home).join(".docker/config.json"));
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            paths.push(PathBuf::from(runtime).join("containers/auth.json"));
        }
        paths.push(PathBuf::from(&home).join(".config/containers/auth.json"));
    }
    paths
}

/// Find a registry in a keyed map, tolerating how the keys are actually written.
///
/// `docker login ghcr.io` writes the bare host, but a file can also carry
/// `https://ghcr.io`, a trailing slash, or Docker Hub's legacy
/// `https://index.docker.io/v1/` spelling — all naming the same registry.
fn lookup<'a, T>(map: &'a BTreeMap<String, T>, registry: &str) -> Option<&'a T> {
    let wanted = normalize_host(registry);
    map.iter()
        .find(|(key, _)| normalize_host(key) == wanted)
        .map(|(_, value)| value)
}

fn normalize_host(key: &str) -> String {
    let stripped = key
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    // Everything up to the first path segment is the host.
    let host = stripped.split('/').next().unwrap_or(stripped);
    match host {
        // The three spellings of Docker Hub, which every config file has an entry for.
        "index.docker.io" | "registry-1.docker.io" | "docker.io" => "docker.io".to_string(),
        other => other.to_lowercase(),
    }
}

fn from_entry(entry: &AuthEntry) -> Option<Credentials> {
    if let (Some(username), Some(password)) = (&entry.username, &entry.password) {
        return Some(Credentials { username: username.clone(), secret: password.clone() });
    }
    let encoded = entry.auth.as_ref()?;
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    // Split on the *first* colon: a password may contain one, a username may not.
    let (username, secret) = text.split_once(':')?;
    if secret.is_empty() {
        return None;
    }
    Some(Credentials { username: username.to_string(), secret: secret.to_string() })
}

/// Ask a credential helper, the way the runtimes do: `docker-credential-<name> get`, registry
/// on stdin, JSON on stdout.
///
/// A helper that does not know the registry exits non-zero or answers with an empty secret;
/// both mean "no credentials", not "fail the build" — the pull may well be anonymous.
fn from_helper(name: &str, registry: &str) -> Option<Credentials> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[derive(Deserialize)]
    struct HelperReply {
        #[serde(rename = "Username")]
        username: Option<String>,
        #[serde(rename = "Secret")]
        secret: Option<String>,
    }

    let mut child = Command::new(format!("docker-credential-{name}"))
        .arg("get")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.as_mut()?.write_all(registry.as_bytes()).ok()?;
    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }

    let reply: HelperReply = serde_json::from_slice(&output.stdout).ok()?;
    let secret = reply.secret.filter(|s| !s.is_empty())?;
    Some(Credentials { username: reply.username.unwrap_or_default(), secret })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Serialises the tests that point the lookup at a temp directory.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Point every auth-file path at `dir`, so the developer's real credentials are never read.
    fn with_auth_file(body: &str, f: impl FnOnce()) {
        let _g = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("auth.json");
        std::fs::write(&path, body).unwrap();
        let previous = std::env::var("REGISTRY_AUTH_FILE").ok();
        let home = std::env::var("HOME").ok();
        let docker = std::env::var("DOCKER_CONFIG").ok();
        std::env::set_var("REGISTRY_AUTH_FILE", &path);
        std::env::remove_var("HOME");
        std::env::remove_var("DOCKER_CONFIG");

        f();

        match previous {
            Some(v) => std::env::set_var("REGISTRY_AUTH_FILE", v),
            None => std::env::remove_var("REGISTRY_AUTH_FILE"),
        }
        if let Some(v) = home {
            std::env::set_var("HOME", v);
        }
        if let Some(v) = docker {
            std::env::set_var("DOCKER_CONFIG", v);
        }
    }

    #[test]
    fn reads_the_base64_form_docker_login_writes() {
        // "user:pa:ss" — the password contains a colon, which the split must not eat.
        with_auth_file(r#"{"auths":{"ghcr.io":{"auth":"dXNlcjpwYTpzcw=="}}}"#, || {
            let found = look_up("ghcr.io").expect("credentials");
            assert_eq!(found.username, "user");
            assert_eq!(found.secret, "pa:ss");
        });
    }

    #[test]
    fn reads_the_plain_username_and_password_form() {
        with_auth_file(r#"{"auths":{"ghcr.io":{"username":"u","password":"p"}}}"#, || {
            assert_eq!(
                look_up("ghcr.io"),
                Some(Credentials { username: "u".into(), secret: "p".into() })
            );
        });
    }

    #[test]
    fn matches_however_the_registry_is_spelled() {
        with_auth_file(r#"{"auths":{"https://ghcr.io/":{"auth":"dTpw"}}}"#, || {
            assert!(look_up("ghcr.io").is_some(), "scheme and trailing slash must not matter");
        });
    }

    #[test]
    fn the_three_spellings_of_docker_hub_are_one_registry() {
        with_auth_file(
            r#"{"auths":{"https://index.docker.io/v1/":{"auth":"dTpw"}}}"#,
            || {
                assert!(look_up("docker.io").is_some());
                assert!(look_up("registry-1.docker.io").is_some());
            },
        );
    }

    #[test]
    fn an_unknown_registry_stays_anonymous() {
        // Not an error: most Features are public, and demanding credentials for them would
        // break the common case to serve the rare one.
        with_auth_file(r#"{"auths":{"ghcr.io":{"auth":"dTpw"}}}"#, || {
            assert_eq!(look_up("registry.example.com"), None);
        });
    }

    #[test]
    fn an_entry_with_an_empty_secret_is_not_credentials() {
        // `docker logout` can leave the key behind with nothing in it.
        with_auth_file(r#"{"auths":{"ghcr.io":{"auth":"dXNlcjo="}}}"#, || {
            assert_eq!(look_up("ghcr.io"), None);
        });
    }

    #[test]
    fn a_malformed_auth_file_is_ignored_rather_than_fatal() {
        with_auth_file("{ not json", || {
            assert_eq!(look_up("ghcr.io"), None);
        });
    }

    /// Install a fake `docker-credential-<name>` on PATH and run `f`.
    #[cfg(unix)]
    fn with_helper(name: &str, body: &str, auth_file: &str, f: impl FnOnce()) {
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(format!("docker-credential-{name}"));
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin, PermissionsExt::from_mode(0o755)).unwrap();
        let file = tmp.path().join("auth.json");
        std::fs::write(&file, auth_file).unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        let old_auth = std::env::var("REGISTRY_AUTH_FILE").ok();
        let old_home = std::env::var("HOME").ok();
        std::env::set_var("PATH", format!("{}:{old_path}", tmp.path().display()));
        std::env::set_var("REGISTRY_AUTH_FILE", &file);
        std::env::remove_var("HOME");

        f();

        std::env::set_var("PATH", old_path);
        match old_auth {
            Some(v) => std::env::set_var("REGISTRY_AUTH_FILE", v),
            None => std::env::remove_var("REGISTRY_AUTH_FILE"),
        }
        if let Some(v) = old_home {
            std::env::set_var("HOME", v);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_credential_helper_is_asked_for_the_registry() {
        // How a login survives on a machine with a keychain — skipping helpers would mean
        // "works until you use Docker Desktop".
        with_helper(
            "test",
            r#"read server; echo "{\"ServerURL\":\"$server\",\"Username\":\"u\",\"Secret\":\"s\"}""#,
            r#"{"credHelpers":{"ghcr.io":"test"}}"#,
            || {
                assert_eq!(
                    look_up("ghcr.io"),
                    Some(Credentials { username: "u".into(), secret: "s".into() })
                );
            },
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_helper_that_knows_nothing_leaves_the_pull_anonymous() {
        // Helpers exit non-zero for a registry they have no entry for. That is "no
        // credentials", not "fail the build" — the Feature may well be public.
        with_helper("test", "exit 1", r#"{"credsStore":"test"}"#, || {
            assert_eq!(look_up("ghcr.io"), None);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_per_registry_helper_wins_over_the_default_store() {
        with_helper(
            "specific",
            r#"read server; echo "{\"Username\":\"right\",\"Secret\":\"s\"}""#,
            r#"{"credsStore":"missing-helper","credHelpers":{"ghcr.io":"specific"}}"#,
            || {
                assert_eq!(look_up("ghcr.io").unwrap().username, "right");
            },
        );
    }

    /// The memo is what stops a keychain-backed helper prompting once per Feature.
    #[cfg(unix)]
    #[test]
    fn credentials_are_looked_up_once_per_registry() {
        use std::os::unix::fs::PermissionsExt;
        let _g = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let calls = tmp.path().join("calls");
        let bin = tmp.path().join("docker-credential-counting");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\nread server\necho x >> {}\necho '{{\"Username\":\"u\",\"Secret\":\"s\"}}'\n",
                calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, PermissionsExt::from_mode(0o755)).unwrap();
        let file = tmp.path().join("auth.json");
        // A host no other test touches, so the shared cache cannot be pre-warmed.
        std::fs::write(&file, r#"{"credHelpers":{"memo.example.com":"counting"}}"#).unwrap();

        let old_path = std::env::var("PATH").unwrap_or_default();
        let old_auth = std::env::var("REGISTRY_AUTH_FILE").ok();
        std::env::set_var("PATH", format!("{}:{old_path}", tmp.path().display()));
        std::env::set_var("REGISTRY_AUTH_FILE", &file);

        assert!(for_registry("memo.example.com").is_some());
        assert!(for_registry("memo.example.com").is_some());

        std::env::set_var("PATH", old_path);
        match old_auth {
            Some(v) => std::env::set_var("REGISTRY_AUTH_FILE", v),
            None => std::env::remove_var("REGISTRY_AUTH_FILE"),
        }
        let invocations = std::fs::read_to_string(&calls).unwrap_or_default().lines().count();
        assert_eq!(invocations, 1, "the helper must be asked once, not once per Feature");
    }

    #[test]
    fn the_basic_header_is_the_encoded_pair() {
        let creds = Credentials { username: "user".into(), secret: "pass".into() };
        assert_eq!(creds.basic_header(), "Basic dXNlcjpwYXNz");
    }
}
