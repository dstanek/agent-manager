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
        registry,
        repository,
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
    let build = |token: Option<&str>| {
        let mut req = ureq::get(url);
        if let Some(a) = accept {
            req = req.set("Accept", a);
        }
        if let Some(t) = token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        req
    };

    let response = match build(None).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(401, r)) => {
            let challenge = r.header("WWW-Authenticate").unwrap_or_default().to_string();
            let token = fetch_token(&challenge)?;
            build(Some(&token)).call().map_err(|e| http_error(url, e))?
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

/// Parse a `Bearer realm="…",service="…",scope="…"` challenge and redeem it for a token.
fn fetch_token(challenge: &str) -> Result<String> {
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
        "https://{}/v2/{}/manifests/{}",
        feature.registry, feature.repository, feature.reference()
    );
    let body = get_with_auth(&url, Some(MANIFEST_ACCEPT))?;
    let mut manifest: Manifest = serde_json::from_slice(&body)
        .with_context(|| format!("parsing the OCI manifest for {}", feature.raw))?;
    // The spec identifies an OCI Feature by its *manifest* digest, not its layer's: two
    // manifests can share a layer while differing in metadata or dependencies, and treating
    // those as one Feature would install one and silently drop the other's dependsOn. Computed
    // over the bytes as served, which is what a registry means by a manifest digest.
    manifest.digest = digest_of(&body);
    Ok(manifest)
}

/// The `sha256:…` digest of some bytes.
pub fn digest_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let sum = Sha256::digest(bytes);
    format!("sha256:{}", sum.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Download and unpack a Feature's layer, returning the directory holding its files.
///
/// Cached by content digest under `cache_root`. A digest is immutable, so a cache hit needs no
/// validation — and a moving tag like `:1` still gets picked up, because the digest comes from
/// a manifest fetched fresh on every build.
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
        "https://{}/v2/{}/blobs/{}",
        feature.registry, feature.repository, layer.digest
    );
    let blob = get_with_auth(&url, None)?;

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
}

