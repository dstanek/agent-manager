//! An OCI registry that runs inside the test process.
//!
//! Most of what `am` does with a registry is protocol work — follow a bearer challenge, read a
//! manifest annotation, fetch a layer by digest, notice when the bytes do not match what was
//! asked for. None of that needs a container, a daemon, or a network: it needs something that
//! answers HTTP the way a registry does, including the ways a *broken* registry does.
//!
//! This is that. It binds `127.0.0.1` on an ephemeral port, so a test builds Feature ids like
//! `127.0.0.1:34517/amtest/base:1.0.0` — `oci::parse_ref` reads a host with a colon as a
//! registry, and `oci::scheme` speaks plain HTTP to loopback, so the client under test needs no
//! special casing to reach it.
//!
//! What it deliberately does **not** replace: the differential tests. Their whole value is that
//! the real reference CLI, resolving real Features published to a real registry, agrees with
//! `am`. Pointing those at a fixture would leave them asserting that this file agrees with
//! itself.
//!
//! Two things it can do that no real registry will do on request:
//!
//! - serve content that does not match the digest addressing it, which is the supply-chain case
//!   `verify_digest` exists for;
//! - record every request path, so a test can assert *what was asked for* — the difference
//!   between a build that consulted its lockfile and one that resolved the tag and got lucky.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::oci::digest_of;

/// What the registry demands before it serves anything.
#[derive(Clone, Debug, Default)]
pub enum Auth {
    #[default]
    Anonymous,
    /// Answer with a `Bearer` challenge naming this registry's own token endpoint. The token is
    /// handed to anyone who asks unless `require_credentials` is set, in which case the token
    /// request itself must carry Basic credentials — which is how ghcr and Docker Hub behave
    /// for a private repository.
    Bearer {
        token: String,
        require_credentials: bool,
    },
    /// Answer with a `Basic` challenge and require exactly these credentials, which is what a
    /// plain htpasswd-protected `registry:2` does.
    Basic { user: String, password: String },
}

/// A deliberate malfunction. A registry cannot be asked to misbehave, so tests that need it to
/// have had no way to run at all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fault {
    #[default]
    None,
    /// Serve manifest bytes that do not hash to the digest they were requested by.
    CorruptManifest,
    /// Serve layer bytes that do not hash to the digest they were requested by.
    CorruptBlob,
    /// Answer every manifest request with 404, whatever was asked for.
    MissingManifest,
}

/// One Feature the fixture serves, with its manifest and layer already built.
#[derive(Clone, Debug)]
pub struct Feature {
    pub repository: String,
    pub tag: String,
    metadata: String,
    layer: Vec<u8>,
    layer_digest: String,
    manifest: String,
    manifest_digest: String,
}

impl Feature {
    /// A Feature with the given id and `devcontainer-feature.json` contents.
    ///
    /// The metadata is both packed into the layer and echoed into the manifest annotation, which
    /// is what a real `devcontainer features publish` does — and what lets `am` resolve a
    /// dependency graph without downloading a single layer.
    pub fn new(repository: &str, tag: &str, metadata: &str) -> Self {
        let layer = layer_tar(metadata);
        let layer_digest = digest_of(&layer);
        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
"config":{{"mediaType":"application/vnd.devcontainers","digest":"sha256:{empty}","size":0}},
"layers":[{{"mediaType":"application/vnd.devcontainers.layer.v1+tar","digest":"{layer_digest}","size":{size}}}],
"annotations":{{"dev.containers.metadata":{metadata_json}}}}}"#,
            empty = "0".repeat(64),
            size = layer.len(),
            metadata_json = json_string(metadata),
        );
        let manifest_digest = digest_of(manifest.as_bytes());
        Self {
            repository: repository.to_string(),
            tag: tag.to_string(),
            metadata: metadata.to_string(),
            layer,
            layer_digest,
            manifest,
            manifest_digest,
        }
    }

    /// A minimal Feature: an id, a version, and nothing else.
    pub fn simple(repository: &str, tag: &str, id: &str) -> Self {
        Self::new(
            repository,
            tag,
            &format!(r#"{{"id":"{id}","version":"{tag}","name":"{id}"}}"#),
        )
    }

    /// The digest the registry will serve this Feature's manifest under, which is what a
    /// lockfile pins and what `am` records.
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    pub fn layer_digest(&self) -> &str {
        &self.layer_digest
    }

    pub fn metadata(&self) -> &str {
        &self.metadata
    }
}

/// Build the tar a Feature layer is: its `devcontainer-feature.json` and an `install.sh`.
fn layer_tar(metadata: &str) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, body) in [
        ("devcontainer-feature.json", metadata),
        ("install.sh", "#!/bin/sh\necho installed\n"),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_path(name).expect("fixture entry name is valid");
        header.set_cksum();
        builder.append(&header, body.as_bytes()).expect("building the fixture layer");
    }
    builder.into_inner().expect("finishing the fixture layer")
}

/// Escape a string as a JSON string literal, for embedding the metadata in the annotation.
fn json_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Builds a [`FakeRegistry`]. Every knob defaults to "behaves like a registry should".
pub struct Builder {
    features: Vec<Feature>,
    auth: Auth,
    fault: Fault,
}

impl Builder {
    pub fn feature(mut self, feature: Feature) -> Self {
        self.features.push(feature);
        self
    }

    pub fn auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    pub fn fault(mut self, fault: Fault) -> Self {
        self.fault = fault;
        self
    }

    pub fn start(self) -> FakeRegistry {
        let Builder { features, auth, fault } = self;
        FakeRegistry::spawn(TcpListener::bind("127.0.0.1:0").expect("bind"), features, auth, fault)
    }

    /// Start, letting the Features be built from the address the registry just bound.
    ///
    /// A `dependsOn` entry has to name the dependency by its full id, port included, which is
    /// not known until the listener exists. Binding first and building second is the only
    /// honest way to express that — the alternative is guessing a port and skipping the test
    /// when the guess is wrong, which is a test that reports success for having done nothing.
    pub fn start_with(self, build: impl FnOnce(&str) -> Vec<Feature>) -> FakeRegistry {
        let Builder { mut features, auth, fault } = self;
        let listener = TcpListener::bind("127.0.0.1:0").expect("binding the fixture registry");
        let host = listener.local_addr().expect("fixture registry address").to_string();
        features.extend(build(&host));
        FakeRegistry::spawn(listener, features, auth, fault)
    }
}

/// A registry serving canned Features over HTTP on loopback.
pub struct FakeRegistry {
    addr: SocketAddr,
    features: Vec<Feature>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl FakeRegistry {
    pub fn builder() -> Builder {
        Builder {
            features: Vec::new(),
            auth: Auth::Anonymous,
            fault: Fault::None,
        }
    }

    /// The shortest useful setup: one anonymous Feature, no faults.
    pub fn with_feature(feature: Feature) -> Self {
        Self::builder().feature(feature).start()
    }

    fn spawn(listener: TcpListener, features: Vec<Feature>, auth: Auth, fault: Fault) -> Self {
        let addr = listener.local_addr().expect("fixture registry address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));

        let handle = {
            let features = features.clone();
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    let Ok(stream) = stream else { continue };
                    // One request per connection: every response says `Connection: close`, so
                    // the client never tries to reuse one.
                    handle_connection(stream, &features, &auth, fault, &requests);
                }
            })
        };

        Self {
            addr,
            features,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    /// `127.0.0.1:<port>` — the registry half of a Feature id.
    pub fn host(&self) -> String {
        self.addr.to_string()
    }

    /// A full Feature id addressing this registry, as a `devcontainer.json` would write it.
    pub fn id(&self, repository: &str, tag: &str) -> String {
        format!("{}/{repository}:{tag}", self.host())
    }

    /// The same, pinned to a digest rather than a tag.
    pub fn id_at_digest(&self, repository: &str, digest: &str) -> String {
        format!("{}/{repository}@{digest}", self.host())
    }

    pub fn feature(&self, repository: &str) -> &Feature {
        self.features
            .iter()
            .find(|f| f.repository == repository)
            .unwrap_or_else(|| panic!("no fixture Feature for {repository}"))
    }

    /// Every request path the registry has answered, in order.
    ///
    /// This is what makes "did the build consult its lockfile?" answerable: a pinned build asks
    /// for `/manifests/sha256:…`, an unpinned one asks for `/manifests/1.0.0`.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request log").clone()
    }

    /// Whether any request was made for the given path fragment.
    pub fn requested(&self, fragment: &str) -> bool {
        self.requests().iter().any(|r| r.contains(fragment))
    }
}

impl Drop for FakeRegistry {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // `incoming()` blocks in accept(), so it has to be woken before it will see the flag.
        let _ = TcpStream::connect(self.addr);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(
    mut stream: TcpStream,
    features: &[Feature],
    auth: &Auth,
    fault: Fault,
    requests: &Arc<Mutex<Vec<String>>>,
) {
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();

    // Headers, read only for the Authorization the auth modes care about.
    let mut authorization = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.trim().is_empty() {
                    break;
                }
                if let Some(value) = line.strip_prefix("Authorization: ") {
                    authorization = Some(value.trim().to_string());
                }
            }
            Err(_) => break,
        }
    }

    requests.lock().expect("request log").push(path.clone());

    // The token endpoint is part of the fixture, so a bearer challenge can be followed all the
    // way through rather than stubbed at the first 401.
    if path.starts_with("/token") {
        if let Auth::Bearer {
            token,
            require_credentials,
        } = auth
        {
            if *require_credentials && authorization.is_none() {
                respond(&mut stream, 401, "application/json", br#"{"errors":[]}"#, None);
                return;
            }
            let body = format!(r#"{{"token":"{token}"}}"#);
            respond(&mut stream, 200, "application/json", body.as_bytes(), None);
            return;
        }
        respond(&mut stream, 404, "application/json", b"{}", None);
        return;
    }

    if let Some(challenge) = unmet_challenge(auth, authorization.as_deref()) {
        respond(&mut stream, 401, "application/json", br#"{"errors":[]}"#, Some(&challenge));
        return;
    }

    // `/v2/` is the API version probe every client makes.
    if path == "/v2/" || path == "/v2" {
        respond(&mut stream, 200, "application/json", b"{}", None);
        return;
    }

    if let Some((repository, reference)) = split_path(&path, "/manifests/") {
        if fault == Fault::MissingManifest {
            respond(&mut stream, 404, "application/json", br#"{"errors":[{"code":"MANIFEST_UNKNOWN"}]}"#, None);
            return;
        }
        let found = features.iter().find(|f| {
            f.repository == repository && (f.tag == reference || f.manifest_digest == reference)
        });
        match found {
            Some(feature) if fault == Fault::CorruptManifest => {
                // Valid JSON, wrong bytes: the client must notice via the digest, not by
                // failing to parse.
                let corrupted = feature.manifest.replacen("\"size\":0", "\"size\":1", 1);
                respond(
                    &mut stream,
                    200,
                    "application/vnd.oci.image.manifest.v1+json",
                    corrupted.as_bytes(),
                    None,
                );
            }
            Some(feature) => respond(
                &mut stream,
                200,
                "application/vnd.oci.image.manifest.v1+json",
                feature.manifest.as_bytes(),
                None,
            ),
            None => respond(
                &mut stream,
                404,
                "application/json",
                br#"{"errors":[{"code":"MANIFEST_UNKNOWN"}]}"#,
                None,
            ),
        }
        return;
    }

    if let Some((repository, digest)) = split_path(&path, "/blobs/") {
        let found = features
            .iter()
            .find(|f| f.repository == repository && f.layer_digest == digest);
        match found {
            Some(_) if fault == Fault::CorruptBlob => {
                respond(&mut stream, 200, "application/octet-stream", b"not the layer", None)
            }
            Some(feature) => respond(
                &mut stream,
                200,
                "application/octet-stream",
                &feature.layer,
                None,
            ),
            None => respond(
                &mut stream,
                404,
                "application/json",
                br#"{"errors":[{"code":"BLOB_UNKNOWN"}]}"#,
                None,
            ),
        }
        return;
    }

    respond(&mut stream, 404, "application/json", b"{}", None);
}

/// The `WWW-Authenticate` value to answer with, or `None` when the request may proceed.
fn unmet_challenge(auth: &Auth, authorization: Option<&str>) -> Option<String> {
    match auth {
        Auth::Anonymous => None,
        Auth::Bearer { token, .. } => {
            let expected = format!("Bearer {token}");
            if authorization == Some(expected.as_str()) {
                None
            } else {
                // The realm points back at this same fixture, so the client's challenge-follow
                // is exercised rather than short-circuited.
                Some(r#"Bearer realm="http://REALM/token",service="fixture",scope="pull""#.to_string())
            }
        }
        Auth::Basic { user, password } => {
            let expected = format!(
                "Basic {}",
                base64_encode(format!("{user}:{password}").as_bytes())
            );
            if authorization == Some(expected.as_str()) {
                None
            } else {
                Some(r#"Basic realm="fixture""#.to_string())
            }
        }
    }
}

/// Split `/v2/<repository><marker><reference>` into its two halves.
fn split_path(path: &str, marker: &str) -> Option<(String, String)> {
    let rest = path.strip_prefix("/v2/")?;
    let (repository, reference) = rest.split_once(marker.trim_start_matches('/'))?;
    Some((
        repository.trim_end_matches('/').to_string(),
        reference.trim_start_matches('/').to_string(),
    ))
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8], challenge: Option<&str>) {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(challenge) = challenge {
        // The realm has to name the port this fixture actually bound, which is only known once
        // the listener exists — so it is patched in here rather than baked into the constant.
        let challenge = match stream.local_addr() {
            Ok(addr) => challenge.replace("REALM", &addr.to_string()),
            Err(_) => challenge.to_string(),
        };
        head.push_str(&format!("WWW-Authenticate: {challenge}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// Base64, for the Basic credentials the fixture compares against.
///
/// Hand-rolled because the crate graph has no base64 dependency and adding one to serve twelve
/// bytes of test input would be a poor trade.
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// Set an environment variable for the life of the value, restoring whatever was there.
pub struct EnvVarGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvVarGuard {
    pub fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// Point `AM_FEATURE_CACHE` at a disposable directory for the life of the value.
///
/// Without it `fetch_layer` caches by digest under the developer's real cache directory, where
/// one test's layer can satisfy another's fetch — and a cache-hit assertion passes without the
/// code that populates the cache ever running. Restoring on drop rather than at the end of the
/// test matters because a panicking test would otherwise leak the variable into every test
/// that follows it (the suite runs single-threaded, so that is every test).
pub struct CacheDir {
    _tmp: tempfile::TempDir,
    previous: Option<String>,
}

impl CacheDir {
    pub fn new() -> Self {
        let tmp = tempfile::tempdir().expect("feature cache dir");
        let previous = std::env::var("AM_FEATURE_CACHE").ok();
        std::env::set_var("AM_FEATURE_CACHE", tmp.path());
        Self { _tmp: tmp, previous }
    }

}

impl Drop for CacheDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => std::env::set_var("AM_FEATURE_CACHE", v),
            None => std::env::remove_var("AM_FEATURE_CACHE"),
        }
    }
}
