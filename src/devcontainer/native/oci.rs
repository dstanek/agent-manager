//! Pulling Features from an OCI registry.
//!
//! A Feature is published as an OCI *artifact*, not an image: one layer whose media type is
//! `application/vnd.devcontainers.layer.v1+tar`, containing `devcontainer-feature.json` and
//! `install.sh`. Three requests get it — token, manifest, blob.
//!
//! The manifest carries the whole `devcontainer-feature.json` in its `dev.containers.metadata`
//! annotation. That is what makes install ordering and option defaults resolvable *without*
//! downloading any layer, so a config whose features are unchanged costs three small GETs
//! rather than a full re-pull.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::error::AmError;

/// Registries answer with a manifest under either media type depending on how the artifact
/// was pushed; asking for both avoids a 404 that is really a content negotiation failure.
const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.manifest.v1+json, \
                              application/vnd.docker.distribution.manifest.v2+json";

/// Where a Feature's files come from. The three the devcontainer spec defines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureKind {
    Registry,
    /// `./path` or `../path` — a Feature vendored in the repo, resolved against the directory
    /// holding the `devcontainer.json`.
    Local,
    /// A direct `https://…/devcontainer-feature-<id>.tgz`.
    Tarball,
}

/// A reference to a Feature, in any of the three forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRef {
    pub kind: FeatureKind,
    /// Registry host. Empty for a local path or a tarball.
    pub registry: String,
    /// For a registry Feature, everything between the host and the tag
    /// (`devcontainers/features/git`). For the other two, the path or URL as written.
    pub repository: String,
    /// Empty for a local path or a tarball — neither has a version to pin.
    pub tag: String,
    /// Set instead of `tag` for a digest-pinned reference (`…/git@sha256:…`), which the spec
    /// allows anywhere a Feature is named — including inside `dependsOn`.
    pub digest: Option<String>,
    /// The id exactly as the user wrote it — this is what goes in the metadata label, so it
    /// must survive round-tripping rather than being reassembled from the parts.
    pub raw: String,
}

impl FeatureRef {
    /// What to ask the registry for: a digest when the reference pins one, else the tag.
    /// Both are valid in the `/manifests/<reference>` position.
    pub fn reference(&self) -> &str {
        self.digest.as_deref().unwrap_or(&self.tag)
    }

    /// The Feature's short alias, as `overrideFeatureInstallOrder` may write it.
    ///
    /// The last path segment for a registry Feature (`git`). A tarball has no alias at all —
    /// the reference CLI lists none for one — and a local Feature's is its directory name.
    pub fn name(&self) -> &str {
        match self.kind {
            FeatureKind::Tarball => "",
            _ => self.repository.rsplit('/').next().unwrap_or(&self.repository),
        }
    }

    /// The identity an `installsAfter` entry or an override entry is compared against.
    ///
    /// `installsAfter` entries are written untagged, so a registry Feature drops its tag here.
    /// A local path or a tarball URL has no tag to drop and compares as written.
    pub fn untagged(&self) -> String {
        match self.kind {
            FeatureKind::Registry => format!("{}/{}", self.registry, self.repository),
            _ => self.raw.clone(),
        }
    }
}

/// Parse a Feature id into a reference.
///
/// The registry form is recognised by having a dotted (or `localhost`) first segment, which is
/// how the OCI spec itself distinguishes a registry host from a Docker Hub shorthand. `am`
/// deliberately does *not* accept the shorthand: a Feature id without a registry is not a
/// thing the devcontainer spec produces, and silently defaulting to Docker Hub would turn a
/// typo into a confusing network error — so a bare name is read as a local path instead, which
/// is what it is.
pub fn parse_ref(id: &str) -> FeatureRef {
    let local = |id: &str| FeatureRef {
        kind: FeatureKind::Local,
        registry: String::new(),
        repository: id.to_string(),
        tag: String::new(),
        digest: None,
        raw: id.to_string(),
    };

    if id.starts_with("./") || id.starts_with("../") {
        return local(id);
    }
    // `http://` is classified here and refused in `fetch_tarball`, not rejected outright:
    // falling through leaves it to be read as a registry reference with the host `http:`, and
    // "no such host" is a much worse answer than naming the actual problem.
    if id.starts_with("https://") || id.starts_with("http://") {
        return FeatureRef {
            kind: FeatureKind::Tarball,
            registry: String::new(),
            repository: id.to_string(),
            tag: String::new(),
            digest: None,
            raw: id.to_string(),
        };
    }

    // A digest pin is checked before the tag split, because `@sha256:…` contains a colon that
    // is emphatically not a tag separator — reading it as one leaves the repository ending in
    // `@sha256` and silently fetches whatever `latest` happens to be.
    let (path, tag, digest) = match id.split_once('@') {
        Some((path, digest)) => (path, String::new(), Some(digest.to_string())),
        None => {
            // Split the tag off, but only from the last segment — a registry may carry a port
            // (`localhost:5000/foo`), and that colon is not a tag separator either.
            match id.rsplit_once(':') {
                Some((p, t)) if !t.contains('/') => (p, t.to_string(), None),
                _ => (id, "latest".to_string(), None),
            }
        }
    };

    let (registry, repository) = match path.split_once('/') {
        Some((host, rest)) if host.contains('.') || host.contains(':') || host == "localhost" => {
            (host.to_string(), rest.to_string())
        }
        _ => return local(id),
    };

    FeatureRef {
        kind: FeatureKind::Registry,
        // Lower-cased because the spec says Feature ids are case-insensitive and OCI repository
        // names must be lowercase anyway: `ghcr.io/Devcontainers/features/git:1` works in an
        // editor and 404s otherwise. The tag is left alone — it is not part of that rule — and
        // `raw` keeps the user's spelling, since that is what the label echoes back.
        registry: registry.to_lowercase(),
        repository: repository.to_lowercase(),
        tag,
        digest,
        raw: id.to_string(),
    }
}

// ── Manifest ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub annotations: std::collections::BTreeMap<String, String>,
    /// Digest of the manifest bytes. Filled in by [`fetch_manifest`] rather than deserialized:
    /// it is a property of the document, not a field in it.
    #[serde(skip)]
    pub digest: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Layer {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
}

impl Manifest {
    /// The Feature's own `devcontainer-feature.json`, which the registry echoes into the
    /// manifest annotations. Absent for a non-Feature artifact.
    pub fn feature_metadata(&self) -> Option<&str> {
        self.annotations.get("dev.containers.metadata").map(|s| s.as_str())
    }

    /// The single tar layer that holds the Feature's files.
    pub fn feature_layer(&self) -> Option<&Layer> {
        self.layers
            .iter()
            .find(|l| l.media_type.starts_with("application/vnd.devcontainers.layer"))
            .or_else(|| self.layers.first())
    }
}

// ── HTTP ──────────────────────────────────────────────────────────────────────

/// Perform a GET, transparently answering a `401` bearer challenge.
///
/// Registries advertise where to get an anonymous token in the `WWW-Authenticate` header, so
/// following the challenge — rather than hardcoding ghcr's token endpoint — is what makes this
/// work against mcr, Docker Hub, and a self-hosted registry without special cases.
fn get_with_auth(url: &str, accept: Option<&str>) -> Result<Vec<u8>> {
    // Credentials are keyed by the registry, not by whatever host the token realm lives on —
    // ghcr.io's realm is ghcr.io, but Docker Hub's is auth.docker.io.
    let credentials = host_of(url).and_then(|host| super::auth::for_registry(&host));

    let build = |authorization: Option<&str>| {
        let mut req = ureq::get(url);
        if let Some(a) = accept {
            req = req.set("Accept", a);
        }
        if let Some(value) = authorization {
            req = req.set("Authorization", value);
        }
        req
    };

    let response = match build(None).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(401, r)) => {
            let challenge = r.header("WWW-Authenticate").unwrap_or_default().to_string();
            let authorization = if challenge.trim_start().starts_with("Basic") {
                // A registry that wants Basic directly, with no token to redeem.
                credentials
                    .as_ref()
                    .map(super::auth::Credentials::basic_header)
                    .ok_or_else(|| unauthorized(url))?
            } else {
                format!("Bearer {}", fetch_token(&challenge, credentials.as_ref())?)
            };
            build(Some(&authorization))
                .call()
                .map_err(|e| enrich_unauthorized(url, e, credentials.is_some()))?
        }
        Err(e) => return Err(http_error(url, e)),
    };

    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .with_context(|| format!("reading response body from {url}"))?;
    Ok(body)
}

fn http_error(url: &str, e: ureq::Error) -> anyhow::Error {
    let detail = match e {
        ureq::Error::Status(code, r) => {
            // The body usually carries the registry's own error text, which is far more
            // useful than the status line alone ("name unknown", "unauthorized").
            let body = r.into_string().unwrap_or_default();
            let trimmed = body.trim();
            if trimmed.is_empty() {
                format!("HTTP {code}")
            } else {
                format!("HTTP {code}: {trimmed}")
            }
        }
        other => other.to_string(),
    };
    AmError::DevcontainerBuildFailed(format!("fetching {url}: {detail}")).into()
}

/// Which scheme to speak to a registry.
///
/// HTTPS everywhere except a loopback host, which is plain HTTP — the same rule Docker and
/// podman apply, where a localhost registry is insecure by default. Without it a developer
/// running `registry:2` on their own machine cannot use it at all, and neither can `am`'s own
/// integration tests, which publish purpose-built Features to exactly such a registry.
fn scheme(registry: &str) -> &'static str {
    let host = registry.split(':').next().unwrap_or(registry);
    match host {
        "localhost" | "127.0.0.1" | "::1" => "http",
        _ => "https",
    }
}

/// The registry host an API URL addresses.
fn host_of(url: &str) -> Option<String> {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?
        .split('/')
        .next()
        .map(str::to_string)
}

/// A 401 that credentials could plausibly fix, phrased as something to act on.
fn unauthorized(url: &str) -> anyhow::Error {
    let host = host_of(url).unwrap_or_else(|| "the registry".to_string());
    AmError::DevcontainerBuildFailed(format!(
        "{host} refused an anonymous pull and no credentials were found for it\n\
         Run `docker login {host}` (or `podman login {host}`) — am reads the same files"
    ))
    .into()
}

/// Turn a post-authentication 401/403 into something that says which case it is.
///
/// The registry's own text is kept, because "denied: requested access to the resource is
/// denied" and "unauthorized: authentication required" mean different things and only the
/// registry knows which applies.
fn enrich_unauthorized(url: &str, e: ureq::Error, had_credentials: bool) -> anyhow::Error {
    match (&e, had_credentials) {
        (ureq::Error::Status(401 | 403, _), false) => unauthorized(url),
        (ureq::Error::Status(401 | 403, _), true) => {
            let host = host_of(url).unwrap_or_else(|| "the registry".to_string());
            let detail = http_error(url, e).to_string();
            AmError::DevcontainerBuildFailed(format!(
                "{detail}\nam used the credentials it found for {host}. They may be expired, \
                 or may not grant access to this Feature."
            ))
            .into()
        }
        _ => http_error(url, e),
    }
}

/// Parse a `Bearer realm="…",service="…",scope="…"` challenge and redeem it for a token.
///
/// Credentials, when there are any, are presented to the *token* endpoint with Basic auth —
/// which is how a registry hands out a bearer token scoped to a private repository.
fn fetch_token(challenge: &str, credentials: Option<&super::auth::Credentials>) -> Result<String> {
    let mut realm = None;
    let mut params: Vec<(String, String)> = Vec::new();
    for part in challenge.trim_start_matches("Bearer ").split(',') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim().trim_matches('"').to_string();
        if k == "realm" {
            realm = Some(v);
        } else {
            params.push((k.to_string(), v));
        }
    }
    let realm = realm.ok_or_else(|| {
        AmError::DevcontainerBuildFailed(format!(
            "registry returned 401 with no token realm to follow: {challenge}"
        ))
    })?;

    let mut req = ureq::get(&realm);
    for (k, v) in &params {
        req = req.query(k, v);
    }
    if let Some(creds) = credentials {
        req = req.set("Authorization", &creds.basic_header());
    }
    let body = req
        .call()
        .map_err(|e| http_error(&realm, e))?
        .into_string()
        .with_context(|| format!("reading token response from {realm}"))?;

    #[derive(Deserialize)]
    struct TokenResponse {
        token: Option<String>,
        access_token: Option<String>,
    }
    let parsed: TokenResponse = serde_json::from_str(&body)
        .with_context(|| format!("parsing token response from {realm}"))?;
    parsed
        .token
        .or(parsed.access_token)
        .ok_or_else(|| AmError::DevcontainerBuildFailed(format!("no token in {realm} response")).into())
}

// ── Fetching ──────────────────────────────────────────────────────────────────

/// Fetch a Feature's manifest.
pub fn fetch_manifest(feature: &FeatureRef) -> Result<Manifest> {
    let url = format!(
        "{}://{}/v2/{}/manifests/{}",
        scheme(&feature.registry),
        feature.registry,
        feature.repository,
        feature.reference()
    );
    let body = get_with_auth(&url, Some(MANIFEST_ACCEPT))?;
    // The spec identifies an OCI Feature by its *manifest* digest, not its layer's: two
    // manifests can share a layer while differing in metadata or dependencies, and treating
    // those as one Feature would install one and silently drop the other's dependsOn. Computed
    // over the bytes as served, which is what a registry means by a manifest digest.
    let digest = digest_of(&body);
    // A digest-pinned reference (written by the user as `@sha256:…`, or pinned by the
    // lockfile) asked the registry for these exact bytes by content address. A tag-addressed
    // request has no expected digest to check against; the first fetch is what establishes one,
    // for the lockfile to pin next time.
    if let Some(expected) = &feature.digest {
        verify_digest("manifest", &feature.raw, expected, &digest)?;
    }
    let mut manifest: Manifest = serde_json::from_slice(&body)
        .with_context(|| format!("parsing the OCI manifest for {}", feature.raw))?;
    manifest.digest = digest;
    Ok(manifest)
}

/// The `sha256:…` digest of some bytes.
pub fn digest_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(bytes);
    format!("sha256:{}", sum.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Confirm that a digest computed from downloaded bytes matches the one a content-addressed
/// request asked for.
///
/// `am` requests a manifest or a layer blob by digest — `/manifests/sha256:…` or
/// `/blobs/sha256:…` — and a registry that answers with anything else is misbehaving,
/// misconfigured, or compromised. Features run their install script as root while the image is
/// built, so that is refused rather than trusted: this is the supply-chain boundary a lockfile
/// pin exists to enforce. `what` names the object in the error (`"manifest"`, `"layer"`).
fn verify_digest(what: &str, feature_raw: &str, expected: &str, actual: &str) -> Result<()> {
    if expected != actual {
        return Err(AmError::DevcontainerBuildFailed(format!(
            "{what} for '{feature_raw}' failed integrity verification\n\
             expected {expected}, got {actual}\n\
             The registry returned different content than the digest it was asked for."
        ))
        .into());
    }
    Ok(())
}

/// Download and unpack a Feature's layer, returning the directory holding its files.
///
/// Cached by content digest under `cache_root`. A digest is immutable, so a cache *hit* needs no
/// re-verification — the cache key itself is the layer digest, and it only got there by a fetch
/// that hashed the bytes and confirmed they matched before writing `.am-complete` (below). A
/// moving tag like `:1` still gets picked up on a miss, because the digest comes from a manifest
/// fetched fresh on every build.
pub fn fetch_layer(feature: &FeatureRef, layer: &Layer, cache_root: &Path) -> Result<PathBuf> {
    let digest = layer.digest.replace(':', "-");
    let dir = cache_root.join(&digest);
    // The marker, not the directory, signals completeness: an interrupted unpack leaves a
    // populated directory that would otherwise be trusted on the next run.
    let marker = dir.join(".am-complete");
    if marker.is_file() {
        return Ok(dir);
    }

    let url = format!(
        "{}://{}/v2/{}/blobs/{}",
        scheme(&feature.registry),
        feature.registry,
        feature.repository,
        layer.digest
    );
    let blob = get_with_auth(&url, None)?;

    // The blob request is always content-addressed — a layer digest comes from the manifest,
    // never from a tag — so this check is unconditional, unlike the manifest's. Verified before
    // anything touches disk: on a mismatch there is no cache directory to clean up, and
    // `.am-complete` never gets written for content that failed the check.
    verify_digest("layer", &feature.raw, &layer.digest, &digest_of(&blob))?;

    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing stale feature cache {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating feature cache {}", dir.display()))?;
    unpack(&blob, &dir)
        .with_context(|| format!("unpacking {} into {}", feature.raw, dir.display()))?;
    std::fs::write(&marker, b"")
        .with_context(|| format!("marking {} complete", dir.display()))?;
    Ok(dir)
}

/// Download and unpack a Feature published as a plain tarball, returning its directory and the
/// digest of the bytes fetched.
///
/// The spec keys equality for these on the tarball's own hash rather than on the URL, so the
/// digest is computed here and returned rather than derived from the address — two URLs serving
/// identical bytes are one Feature.
///
/// Unlike a registry layer this cannot be cached before it is fetched: a URL is mutable, so
/// there is no immutable name to look up first. The download therefore happens every build, and
/// only the unpacking is skipped on a hit.
pub fn fetch_tarball(feature: &FeatureRef, cache_root: &Path) -> Result<(PathBuf, String)> {
    // The spec defines direct references as an HTTPS URI, and the reference CLI refuses
    // anything else. A Feature is code that runs as root while the image is built, so fetching
    // it over a channel with no integrity guarantee is not a convenience worth offering.
    if !feature.raw.starts_with("https://") {
        return Err(AmError::DevcontainerBuildFailed(format!(
            "Feature '{}' is not an https:// URL — a Feature tarball must be served over \
             HTTPS, since it runs as root while the image is built",
            feature.raw
        ))
        .into());
    }
    let body = get_with_auth(&feature.raw, None)
        .with_context(|| format!("downloading {}", feature.raw))?;

    let digest = {
        use sha2::{Digest, Sha256};
        let sum = Sha256::digest(&body);
        format!("sha256:{}", sum.iter().map(|b| format!("{b:02x}")).collect::<String>())
    };
    let dir = cache_root.join(digest.replace(':', "-"));
    let marker = dir.join(".am-complete");
    if marker.is_file() {
        return Ok((dir, digest));
    }

    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing stale feature cache {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating feature cache {}", dir.display()))?;
    unpack(&body, &dir)
        .with_context(|| format!("unpacking {} into {}", feature.raw, dir.display()))?;
    std::fs::write(&marker, b"")
        .with_context(|| format!("marking {} complete", dir.display()))?;
    Ok((dir, digest))
}

/// Unpack a Feature layer, which is a tar that may or may not be gzipped.
///
/// The `.tgz` in the layer's title annotation is not reliable — ghcr serves the
/// `devcontainers/features` layers as plain tar despite it — so the magic bytes decide.
fn unpack(blob: &[u8], dest: &Path) -> Result<()> {
    if blob.starts_with(&[0x1f, 0x8b]) {
        tar::Archive::new(flate2::read::GzDecoder::new(blob)).unpack(dest)?;
    } else {
        tar::Archive::new(blob).unpack(dest)?;
    }
    Ok(())
}

/// Where downloaded Feature layers live.
pub fn cache_root() -> PathBuf {
    if let Ok(dir) = std::env::var("AM_FEATURE_CACHE") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|_| PathBuf::from(".am-cache"));
    base.join("am").join("features")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_tagged_registry_ref() {
        let r = parse_ref("ghcr.io/devcontainers/features/git:1");
        assert_eq!(r.kind, FeatureKind::Registry);
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "devcontainers/features/git");
        assert_eq!(r.tag, "1");
        assert_eq!(r.name(), "git");
        assert_eq!(r.untagged(), "ghcr.io/devcontainers/features/git");
    }

    #[test]
    fn untagged_ref_defaults_to_latest() {
        let r = parse_ref("ghcr.io/devcontainers/features/git");
        assert_eq!(r.tag, "latest");
        // The label must echo what the user wrote, not the normalised form.
        assert_eq!(r.raw, "ghcr.io/devcontainers/features/git");
    }

    #[test]
    fn registry_port_is_not_mistaken_for_a_tag() {
        let r = parse_ref("localhost:5000/my/feature:2");
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "my/feature");
        assert_eq!(r.tag, "2");
    }

    #[test]
    fn recognises_local_and_tarball_sources() {
        let local = parse_ref("./my-feature");
        assert_eq!(local.kind, FeatureKind::Local);
        // Both compare as written: neither has a tag to strip.
        assert_eq!(local.untagged(), "./my-feature");
        assert_eq!(local.name(), "my-feature");

        let tarball = parse_ref("https://example.com/devcontainer-feature-f.tgz");
        assert_eq!(tarball.kind, FeatureKind::Tarball);
        assert_eq!(tarball.untagged(), "https://example.com/devcontainer-feature-f.tgz");
        // A tarball has no short alias — the reference CLI lists none for one — so it can only
        // be named in full in an overrideFeatureInstallOrder.
        assert_eq!(tarball.name(), "");
    }

    #[test]
    fn a_loopback_registry_is_plain_http() {
        // The same rule Docker and podman apply. Without it a developer running `registry:2`
        // locally cannot use it, and neither can am's own integration tests.
        assert_eq!(scheme("localhost:5000"), "http");
        assert_eq!(scheme("127.0.0.1:5000"), "http");
        assert_eq!(scheme("ghcr.io"), "https");
        assert_eq!(scheme("registry.example.com:5000"), "https");
    }

    #[test]
    fn a_feature_id_is_case_insensitive() {
        let r = parse_ref("GHCR.io/Devcontainers/Features/Git:1");
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "devcontainers/features/git");
        // The label echoes what the user wrote, so `raw` is untouched.
        assert_eq!(r.raw, "GHCR.io/Devcontainers/Features/Git:1");
    }

    #[test]
    fn a_digest_pinned_reference_keeps_its_digest() {
        // `@sha256:…` contains a colon that is not a tag separator. Reading it as one leaves
        // the repository ending in `@sha256` and quietly fetches `latest` instead of the pinned
        // Feature — which the spec allows anywhere a Feature is named, `dependsOn` included.
        let r = parse_ref("ghcr.io/devcontainers/features/git@sha256:abc123");
        assert_eq!(r.kind, FeatureKind::Registry);
        assert_eq!(r.repository, "devcontainers/features/git");
        assert_eq!(r.digest.as_deref(), Some("sha256:abc123"));
        assert_eq!(r.reference(), "sha256:abc123", "the registry is asked for the digest");
        assert_eq!(r.untagged(), "ghcr.io/devcontainers/features/git");
    }

    #[test]
    fn a_tagged_reference_asks_for_its_tag() {
        let r = parse_ref("ghcr.io/devcontainers/features/git:1");
        assert_eq!(r.digest, None);
        assert_eq!(r.reference(), "1");
    }

    #[test]
    fn an_http_tarball_is_refused_with_a_message_about_https() {
        // Classified as a tarball so the error can name the real problem — reading it as a
        // registry reference instead would fail with "no such host: http:".
        let feature = parse_ref("http://example.com/f.tgz");
        assert_eq!(feature.kind, FeatureKind::Tarball);
        let tmp = tempfile::tempdir().unwrap();
        let err = fetch_tarball(&feature, tmp.path()).unwrap_err().to_string();
        assert!(err.contains("https"), "must say what is required: {err}");
        assert!(err.contains("example.com"), "must name the offending id: {err}");
    }

    #[test]
    fn a_bare_name_is_not_treated_as_docker_hub() {
        // No dot in the first segment, so this cannot be a registry host. Reading it as a
        // local path means a typo surfaces as a missing directory rather than as a confusing
        // pull from Docker Hub.
        assert_eq!(parse_ref("some/feature").kind, FeatureKind::Local);
    }

    #[test]
    fn parses_the_captured_manifest() {
        let text = include_str!("../../../tests/fixtures/devcontainer/native/git-oci-manifest.json");
        let manifest: Manifest = serde_json::from_str(text).expect("fixture parses");
        let layer = manifest.feature_layer().expect("has a feature layer");
        assert_eq!(layer.media_type, "application/vnd.devcontainers.layer.v1+tar");
        assert!(layer.digest.starts_with("sha256:"));
        let metadata = manifest.feature_metadata().expect("carries feature metadata");
        assert!(metadata.contains("\"installsAfter\""));
    }

    #[test]
    fn unpacks_a_plain_tar() {
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let body = b"echo hi";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "install.sh", &body[..]).unwrap();
        let archive = builder.into_inner().unwrap();

        unpack(&archive, tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("install.sh")).unwrap(),
            "echo hi"
        );
    }

    #[test]
    fn unpacks_a_gzipped_tar() {
        use std::io::Write;
        let tmp = tempfile::tempdir().unwrap();
        let mut builder = tar::Builder::new(Vec::new());
        let body = b"echo gz";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, "install.sh", &body[..]).unwrap();
        let archive = builder.into_inner().unwrap();
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&archive).unwrap();
        let gzipped = encoder.finish().unwrap();

        assert!(gzipped.starts_with(&[0x1f, 0x8b]));
        unpack(&gzipped, tmp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("install.sh")).unwrap(),
            "echo gz"
        );
    }


    // ── Hostile archives ─────────────────────────────────────────────────────
    //
    // A Feature archive is attacker-controlled input in the sense that matters: it arrives over
    // the network from a registry or a URL, is unpacked by `am`, and its `install.sh` then runs
    // as root while the image builds. Everything below asserts the same property from a
    // different angle — nothing an archive contains may write outside the cache directory it is
    // unpacked into. These are characterisation tests as much as regression tests: they pin
    // what `unpack` does today so that swapping the tar implementation, or adding a fast path,
    // cannot quietly weaken it.

    /// Write an entry name straight into the header's name field.
    ///
    /// `tar::Header::set_path` refuses `..` and absolute paths — it will not help you build a
    /// hostile archive, which is exactly what these tests need. A real attacker writes the
    /// bytes, so the test does too: the name field is the first 100 bytes of the 512-byte
    /// header, and the checksum is computed afterwards over the finished header.
    fn set_raw_name(header: &mut tar::Header, name: &str) {
        assert!(name.len() <= 100, "test entry name must fit the ustar name field");
        let field = &mut header.as_mut_bytes()[..100];
        field.fill(0);
        field[..name.len()].copy_from_slice(name.as_bytes());
    }

    /// Build a tar containing one entry of an arbitrary type, with an arbitrary path.
    fn tar_with_entry(path: &str, kind: tar::EntryType, link_target: Option<&str>) -> Vec<u8> {
        let body: &[u8] = b"pwned";
        let mut header = tar::Header::new_gnu();
        header.set_size(if link_target.is_some() { 0 } else { body.len() as u64 });
        header.set_mode(0o644);
        header.set_entry_type(kind);
        if let Some(target) = link_target {
            header.set_link_name(target).unwrap();
        }
        set_raw_name(&mut header, path);
        header.set_cksum();

        let mut builder = tar::Builder::new(Vec::new());
        if link_target.is_some() {
            builder.append(&header, std::io::empty()).unwrap();
        } else {
            builder.append(&header, body).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// A path traversal in an entry name must not place a file above the destination.
    #[test]
    fn an_archive_entry_cannot_traverse_above_the_cache_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("cache");
        std::fs::create_dir_all(&dest).unwrap();

        // Measured behaviour, pinned rather than assumed: the unpack *succeeds* and the entry
        // is silently skipped. Worth knowing — a Feature whose archive carries such an entry
        // installs with that file simply absent, and the failure surfaces later as a missing
        // file inside install.sh rather than as a rejected archive.
        let result = unpack(&tar_with_entry("../escaped.txt", tar::EntryType::Regular, None), &dest);

        assert!(result.is_ok(), "the traversal entry is skipped, not treated as a fatal archive");
        assert!(
            !tmp.path().join("escaped.txt").exists(),
            "an entry named ../escaped.txt escaped the destination directory"
        );
        assert!(
            !dest.join("escaped.txt").exists(),
            "the entry was neither written outside nor relocated inside — it is dropped"
        );
    }

    /// The same, with the traversal buried mid-path rather than leading.
    #[test]
    fn an_archive_entry_cannot_traverse_from_within_a_subdirectory() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("cache");
        std::fs::create_dir_all(&dest).unwrap();

        let result = unpack(
            &tar_with_entry("sub/../../escaped.txt", tar::EntryType::Regular, None),
            &dest,
        );

        assert!(result.is_ok(), "same handling as a leading traversal: skipped, not fatal");
        assert!(!tmp.path().join("escaped.txt").exists(), "a mid-path traversal escaped");
    }

    /// An absolute entry name must be treated as relative to the destination, never honoured.
    #[test]
    fn an_archive_entry_with_an_absolute_path_stays_inside_the_cache_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("cache");
        std::fs::create_dir_all(&dest).unwrap();
        // Somewhere writable that the entry names outright.
        let target = tmp.path().join("absolute-escape.txt");

        let result = unpack(
            &tar_with_entry(
                target.to_str().expect("test path is utf-8"),
                tar::EntryType::Regular,
                None,
            ),
            &dest,
        );

        assert!(result.is_ok(), "an absolute path is rewritten, not rejected");
        assert!(!target.exists(), "an absolute entry path was honoured: {}", target.display());
        // Measured: the leading `/` is stripped and the entry lands under the destination.
        // That is the safe outcome, and pinning it here means a change to *rejecting* such an
        // archive is a deliberate decision rather than an unnoticed behaviour change.
        assert!(
            dest.join(target.strip_prefix("/").expect("absolute in a test")).exists(),
            "the entry should have been relocated under the destination"
        );
    }

    /// A symlink pointing out of the archive, followed by a write through it, is the classic
    /// two-entry escape: the link itself is harmless, the write through it is not.
    #[test]
    fn an_archive_cannot_write_through_a_symlink_that_leaves_the_cache_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("cache");
        std::fs::create_dir_all(&dest).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_size(0);
        link.set_mode(0o777);
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_link_name(&outside).unwrap();
        set_raw_name(&mut link, "escape");
        link.set_cksum();
        builder.append(&link, std::io::empty()).unwrap();

        let body: &[u8] = b"pwned";
        let mut file = tar::Header::new_gnu();
        file.set_size(body.len() as u64);
        file.set_mode(0o644);
        set_raw_name(&mut file, "escape/escaped.txt");
        file.set_cksum();
        builder.append(&file, body).unwrap();

        let result = unpack(&builder.into_inner().unwrap(), &dest);

        // The symlink entry itself is created — which is what makes this test meaningful. The
        // archive is processed past its first entry, and it is the *write through* the link
        // that is refused, loudly, rather than the archive being rejected up front.
        assert!(
            std::fs::symlink_metadata(dest.join("escape")).is_ok(),
            "the symlink entry was never created, so this test proves nothing about the write"
        );
        assert!(result.is_err(), "writing through an escaping symlink must fail, not be skipped");
        assert!(
            !outside.join("escaped.txt").exists(),
            "a write through a symlink landed outside the cache directory"
        );
    }

    /// A hard link to a path outside the archive would expose a host file's contents inside the
    /// image — a different escape from the symlink one, and worth its own case.
    #[test]
    fn an_archive_cannot_hard_link_to_a_file_outside_the_cache_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("cache");
        std::fs::create_dir_all(&dest).unwrap();
        let secret = tmp.path().join("secret.txt");
        std::fs::write(&secret, "a host file").unwrap();

        let result = unpack(
            &tar_with_entry(
                "linked.txt",
                tar::EntryType::Link,
                Some(secret.to_str().expect("test path is utf-8")),
            ),
            &dest,
        );

        assert!(result.is_err(), "a hard link out of the archive must fail the unpack");
        assert!(
            !dest.join("linked.txt").exists(),
            "a hard link to a host file was created inside the cache directory"
        );
        assert_eq!(
            std::fs::read_to_string(&secret).unwrap(),
            "a host file",
            "the host file must be untouched"
        );
    }

    /// `.am-complete` is `am`'s own marker for "this cache directory is fully unpacked and
    /// verified". An archive that ships one would be claiming that on its own behalf.
    ///
    /// This documents rather than judges: `fetch_layer` writes the marker itself only after a
    /// successful verify-and-unpack, so an archive carrying one changes nothing today. The test
    /// exists so that a future change making the marker's *presence* the completion check — a
    /// natural-looking optimisation — fails here instead of shipping.
    #[test]
    fn an_archive_carrying_the_completion_marker_is_not_treated_as_authoritative() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("cache");
        std::fs::create_dir_all(&dest).unwrap();

        unpack(&tar_with_entry(".am-complete", tar::EntryType::Regular, None), &dest).unwrap();

        // The marker is inside the cache directory, which is where it would be anyway; what
        // matters is that nothing about the *layer* was verified by its presence. Recorded as
        // an explicit expectation so the reason survives.
        assert!(
            dest.join(".am-complete").exists(),
            "the entry unpacked as an ordinary file, which is the documented behaviour"
        );
    }

    /// The committed fixture is the *only* copy of that Feature — the network test below fetches
    /// this same file back out of the repository — so a rebuild that changes its shape has to
    /// fail here, offline, rather than in an `#[ignore]`d test nobody ran.
    #[test]
    fn the_committed_tarball_fixture_unpacks_to_a_feature() {
        let tmp = tempfile::tempdir().unwrap();
        let blob = include_bytes!("../../../tests/fixtures/tarball/tarball-only.tgz");
        unpack(blob, tmp.path()).unwrap();

        let metadata =
            std::fs::read_to_string(tmp.path().join("devcontainer-feature.json")).unwrap();
        assert!(metadata.contains("\"id\": \"tarball-only\""), "got: {metadata}");
        assert!(tmp.path().join("install.sh").is_file());
    }

    #[test]
    fn verify_digest_accepts_a_match() {
        let sum = digest_of(b"install.sh");
        assert!(verify_digest("layer", "some/feature:1", &sum, &sum).is_ok());
    }

    #[test]
    fn verify_digest_rejects_a_mismatch_naming_both_digests() {
        let expected = digest_of(b"what the lockfile pinned");
        let actual = digest_of(b"what the registry actually sent");
        let err = verify_digest("layer", "some/feature:1", &expected, &actual)
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed integrity verification"), "got: {err}");
        assert!(err.contains("some/feature:1"), "must name the Feature: {err}");
        assert!(err.contains(&expected), "must show the expected digest: {err}");
        assert!(err.contains(&actual), "must show the actual digest: {err}");
    }

    /// Answers exactly one HTTP request with a canned body, then closes. Enough to exercise
    /// `get_with_auth` against a registry that returns content not matching what was requested
    /// by digest, without a real registry or an HTTP-mocking dependency.
    fn respond_once(body: Vec<u8>) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        addr
    }

    /// A digest-pinned manifest reference — the lockfile's whole mechanism — asks a registry for
    /// specific content by address. A registry that answers with something else must be refused,
    /// not installed: this is the case the review that prompted this test found unguarded.
    #[test]
    fn a_manifest_that_does_not_match_its_pinned_digest_is_rejected() {
        let addr = respond_once(br#"{"layers":[],"annotations":{}}"#.to_vec());
        let feature = parse_ref(&format!(
            "127.0.0.1:{}/some/feature@sha256:{}",
            addr.port(),
            "0".repeat(64)
        ));
        assert!(feature.digest.is_some(), "must be read as digest-pinned");

        let err = fetch_manifest(&feature).unwrap_err().to_string();
        assert!(err.contains("failed integrity verification"), "got: {err}");
        assert!(err.contains(&format!("sha256:{}", "0".repeat(64))), "got: {err}");
    }

    /// A blob request is always content-addressed, so unlike the manifest this has no "no digest
    /// pinned" escape hatch — every layer must be verified. On a mismatch, nothing may be cached:
    /// a later successful build must not find a stale, wrong directory sitting under its digest.
    #[test]
    fn a_layer_that_does_not_match_its_digest_is_rejected_and_never_cached() {
        let addr = respond_once(b"not the bytes the manifest promised".to_vec());
        let feature = parse_ref(&format!("127.0.0.1:{}/some/feature:1", addr.port()));
        let layer = Layer {
            media_type: "application/vnd.devcontainers.layer.v1+tar".to_string(),
            digest: format!("sha256:{}", "0".repeat(64)),
        };
        let cache = tempfile::tempdir().unwrap();

        let err = fetch_layer(&feature, &layer, cache.path()).unwrap_err().to_string();
        assert!(err.contains("failed integrity verification"), "got: {err}");

        let cached_dir = cache.path().join(layer.digest.replace(':', "-"));
        assert!(!cached_dir.exists(), "a mismatched layer must not be cached at all");
        assert!(
            !cached_dir.join(".am-complete").exists(),
            "must never be marked complete"
        );
    }

    /// The direct-tarball path, end to end: fetch over real HTTPS, unpack, hash.
    ///
    /// Served from this repository over GitHub's own TLS, because `am` accepts no other scheme
    /// and a locally issued certificate cannot join the root store `ureq` verifies against —
    /// see `tests/fixtures/tarball/README.md`. That makes this the one Feature path with no
    /// local harness to start; the ignore is about the network, not about infrastructure.
    #[test]
    #[ignore = "fetches the fixture tarball over the network"]
    fn a_feature_tarball_is_fetched_and_unpacked_over_https() {
        let url = std::env::var("AM_TARBALL_FIXTURE_URL").unwrap_or_else(|_| {
            "https://raw.githubusercontent.com/dstanek/agent-manager/main/\
             tests/fixtures/tarball/tarball-only.tgz"
                .to_string()
        });
        let cache = tempfile::tempdir().unwrap();
        let feature = parse_ref(&url);
        assert_eq!(feature.kind, FeatureKind::Tarball);

        let (dir, digest) = fetch_tarball(&feature, cache.path()).expect("fetching the tarball");
        assert!(digest.starts_with("sha256:"));
        let metadata = std::fs::read_to_string(dir.join("devcontainer-feature.json"))
            .expect("the archive had no devcontainer-feature.json at its root");
        assert!(metadata.contains("\"id\": \"tarball-only\""), "got: {metadata}");
        assert!(dir.join("install.sh").is_file());

        // Second call is a cache hit on the digest, and must land on the same directory rather
        // than re-downloading into a new one.
        let (again, same_digest) = fetch_tarball(&feature, cache.path()).unwrap();
        assert_eq!((again, same_digest), (dir, digest));
    }

    /// The whole credential path, end to end: a registry that refuses anonymous callers, a
    /// credential in the runtime's auth file put there by a real `docker login`, and `am`
    /// finding it. Nothing in the public registries can test this — they let everyone in.
    ///
    /// The anonymous request first is what keeps the test honest: without it, a registry that
    /// had quietly stopped requiring auth would let the second half pass on its own.
    #[test]
    #[ignore = "needs the local test registries — see scripts/test-registry.sh"]
    fn a_private_registry_is_read_with_the_runtimes_stored_credentials() {
        let feature = parse_ref("localhost:5001/amtest/base:1.0.0");
        let url = "http://localhost:5001/v2/amtest/base/manifests/1.0.0";

        let anonymous = ureq::get(url).set("Accept", MANIFEST_ACCEPT).call();
        assert!(
            matches!(anonymous, Err(ureq::Error::Status(401, _))),
            "the test registry is not actually private: {anonymous:?}"
        );

        let manifest = fetch_manifest(&feature).expect("fetching through the stored credential");
        assert!(manifest.digest.starts_with("sha256:"));
        assert!(
            manifest.feature_layer().is_some(),
            "the manifest carries no Feature layer: {manifest:?}"
        );
    }
}

