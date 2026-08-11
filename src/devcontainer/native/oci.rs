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

/// A Feature published in a registry, parsed from `<registry>/<namespace...>/<name>[:<tag>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureRef {
    pub registry: String,
    /// Everything between the registry and the tag, e.g. `devcontainers/features/git`.
    pub repository: String,
    pub tag: String,
    /// The id exactly as the user wrote it — this is what goes in the metadata label, so it
    /// must survive round-tripping rather than being reassembled from the parts.
    pub raw: String,
}

impl FeatureRef {
    /// The last path segment, used to name the feature's build directory (`git` → `git_0`).
    pub fn name(&self) -> &str {
        self.repository.rsplit('/').next().unwrap_or(&self.repository)
    }

    /// The id without its tag. `installsAfter` entries are written untagged, so comparisons
    /// between a dependency and an installed Feature have to happen on this form.
    pub fn untagged(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }
}

/// Classify a Feature id. Only the registry form is handled natively; the others are how the
/// caller learns it must fall back rather than guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeatureSource {
    Registry(FeatureRef),
    /// `./path` or `../path` — a Feature vendored in the repo.
    Local(String),
    /// A direct `https://…/feature.tgz`.
    Tarball(String),
}

/// Parse a Feature id into its source kind.
///
/// The registry form is recognised by having a dotted (or `localhost`) first segment, which is
/// how the OCI spec itself distinguishes a registry host from a Docker Hub shorthand. `am`
/// deliberately does *not* accept the shorthand: a Feature id without a registry is not a
/// thing the devcontainer spec produces, and silently defaulting to Docker Hub would turn a
/// typo into a confusing network error.
pub fn parse_ref(id: &str) -> FeatureSource {
    if id.starts_with("./") || id.starts_with("../") {
        return FeatureSource::Local(id.to_string());
    }
    if id.starts_with("https://") || id.starts_with("http://") {
        return FeatureSource::Tarball(id.to_string());
    }

    // Split the tag off first, but only from the last segment — a registry may carry a port
    // (`localhost:5000/foo`), and that colon is not a tag separator.
    let (path, tag) = match id.rsplit_once(':') {
        Some((p, t)) if !t.contains('/') => (p, t.to_string()),
        _ => (id, "latest".to_string()),
    };

    let (registry, repository) = match path.split_once('/') {
        Some((host, rest)) if host.contains('.') || host.contains(':') || host == "localhost" => {
            (host.to_string(), rest.to_string())
        }
        _ => return FeatureSource::Local(id.to_string()),
    };

    FeatureSource::Registry(FeatureRef {
        registry,
        repository,
        tag,
        raw: id.to_string(),
    })
}

// ── Manifest ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub layers: Vec<Layer>,
    #[serde(default)]
    pub annotations: std::collections::BTreeMap<String, String>,
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
        feature.registry, feature.repository, feature.tag
    );
    let body = get_with_auth(&url, Some(MANIFEST_ACCEPT))?;
    serde_json::from_slice(&body)
        .with_context(|| format!("parsing the OCI manifest for {}", feature.raw))
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
        let FeatureSource::Registry(r) = parse_ref("ghcr.io/devcontainers/features/git:1") else {
            panic!("expected a registry ref");
        };
        assert_eq!(r.registry, "ghcr.io");
        assert_eq!(r.repository, "devcontainers/features/git");
        assert_eq!(r.tag, "1");
        assert_eq!(r.name(), "git");
        assert_eq!(r.untagged(), "ghcr.io/devcontainers/features/git");
    }

    #[test]
    fn untagged_ref_defaults_to_latest() {
        let FeatureSource::Registry(r) = parse_ref("ghcr.io/devcontainers/features/git") else {
            panic!("expected a registry ref");
        };
        assert_eq!(r.tag, "latest");
        // The label must echo what the user wrote, not the normalised form.
        assert_eq!(r.raw, "ghcr.io/devcontainers/features/git");
    }

    #[test]
    fn registry_port_is_not_mistaken_for_a_tag() {
        let FeatureSource::Registry(r) = parse_ref("localhost:5000/my/feature:2") else {
            panic!("expected a registry ref");
        };
        assert_eq!(r.registry, "localhost:5000");
        assert_eq!(r.repository, "my/feature");
        assert_eq!(r.tag, "2");
    }

    #[test]
    fn recognises_local_and_tarball_sources() {
        assert_eq!(
            parse_ref("./my-feature"),
            FeatureSource::Local("./my-feature".to_string())
        );
        assert_eq!(
            parse_ref("https://example.com/f.tgz"),
            FeatureSource::Tarball("https://example.com/f.tgz".to_string())
        );
    }

    #[test]
    fn a_bare_name_is_not_treated_as_docker_hub() {
        // No dot in the first segment, so this cannot be a registry host. Falling through to
        // Local means the caller falls back to the CLI instead of pulling from Docker Hub.
        assert!(matches!(parse_ref("some/feature"), FeatureSource::Local(_)));
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
