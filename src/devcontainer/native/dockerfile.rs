//! Generating the build context and Dockerfile that installs Features into the base image.
//!
//! The reference CLI builds a throwaway `FROM scratch` "content" image and copies Features out
//! of it across three stages. That indirection exists because the CLI must work with whatever
//! build context the user's own Dockerfile declares. `am` resolves the base image *first* and
//! then owns the context outright, so one stage copying straight from the context is
//! equivalent — and one fewer `docker build` per session.
//!
//! What is *not* simplified is the install contract, because Feature authors depend on it:
//!
//! ```text
//! /tmp/dev-container-features/devcontainer-features.builtin.env   _CONTAINER_USER, _REMOTE_USER, …
//! /tmp/dev-container-features/<name>_<n>/install.sh               the Feature's own script
//! /tmp/dev-container-features/<name>_<n>/devcontainer-features.env  resolved options
//! /tmp/dev-container-features/<name>_<n>/devcontainer-features-install.sh  generated wrapper
//! ```
//!
//! The wrapper sources both env files with `set -a` so they become exported variables, then
//! runs `install.sh` — matching `cli-git_0-install-wrapper.sh` in the fixtures.

use std::path::Path;

use anyhow::{Context, Result};

use super::feature::ResolvedFeature;

/// Where Features land inside the image during the build.
const FEATURES_DIR: &str = "/tmp/dev-container-features";

/// Write the build context: one directory per Feature plus the builtin env file.
pub fn write_context(
    context: &Path,
    features: &[ResolvedFeature],
    cached_dirs: &[std::path::PathBuf],
    container_user: &str,
    remote_user: &str,
) -> Result<()> {
    std::fs::create_dir_all(context)
        .with_context(|| format!("creating build context {}", context.display()))?;

    // The two users the install contract exposes. `_CONTAINER_USER_HOME`/`_REMOTE_USER_HOME`
    // are appended inside the image, where /etc/passwd can actually be read.
    std::fs::write(
        context.join("devcontainer-features.builtin.env"),
        format!("_CONTAINER_USER={container_user}\n_REMOTE_USER={remote_user}\n"),
    )
    .with_context(|| "writing devcontainer-features.builtin.env")?;

    for (index, (feature, source)) in features.iter().zip(cached_dirs).enumerate() {
        let dir = context.join(feature.dir_name(index));
        copy_dir(source, &dir)
            .with_context(|| format!("staging {} into the build context", feature.reference.raw))?;
        std::fs::write(dir.join("devcontainer-features.env"), options_env(feature))
            .with_context(|| format!("writing options for {}", feature.reference.raw))?;
        std::fs::write(
            dir.join("devcontainer-features-install.sh"),
            install_wrapper(feature),
        )
        .with_context(|| format!("writing the install wrapper for {}", feature.reference.raw))?;
    }
    Ok(())
}

/// Copy a cached Feature directory into the build context.
///
/// Recursive by hand rather than by shelling out: the trees are a handful of small files, and
/// this keeps the builder free of another external command.
fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)
        .with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from)
        .with_context(|| format!("reading {}", from.display()))?
    {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Render resolved options as the `KEY="value"` env file `install.sh` sources.
fn options_env(feature: &ResolvedFeature) -> String {
    feature
        .options
        .iter()
        // Escape only what would terminate or re-open the quoted value; Feature option values
        // are arbitrary strings and a stray quote must not become shell syntax.
        .map(|(k, v)| format!("{k}=\"{}\"\n", v.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect()
}

/// The generated script that sets up the environment and runs the Feature's `install.sh`.
fn install_wrapper(feature: &ResolvedFeature) -> String {
    let meta = &feature.metadata;
    let name = meta
        .name
        .clone()
        .unwrap_or_else(|| feature.reference.raw.clone());
    let id = feature.reference.raw.clone();
    let version = meta.version.clone().unwrap_or_else(|| "unknown".to_string());
    let description = meta.description.clone().unwrap_or_default();
    // A failing install is the moment this script's output gets read, so the docs link is
    // worth carrying through exactly as the reference CLI does.
    let docs = meta.documentation_url.clone().unwrap_or_default();
    let troubleshooting = if docs.is_empty() {
        String::new()
    } else {
        format!(" Look at the documentation at {docs} for help troubleshooting this error.")
    };
    let options: String = feature
        .options
        .iter()
        .map(|(k, v)| format!("    {k}=\"{v}\"\n"))
        .collect();

    format!(
        r#"#!/bin/sh
set -e

on_exit () {{
	[ $? -eq 0 ] && exit
	echo 'ERROR: Feature "{name}" ({id}) failed to install!{troubleshooting}'
}}

trap on_exit EXIT

echo ===========================================================================

echo 'Feature       : {name}'
echo 'Description   : {description}'
echo 'Id            : {id}'
echo 'Version       : {version}'
echo 'Documentation : {docs}'
echo 'Options       :'
echo '{options}'
echo ===========================================================================

set -a
. ../devcontainer-features.builtin.env
. ./devcontainer-features.env
set +a

chmod +x ./install.sh
./install.sh
"#
    )
}

/// A Feature's `containerEnv` as `ENV` instructions.
///
/// Values are *not* `$`-escaped, unlike the metadata label: `"PATH": "/opt/tool/bin:${PATH}"` is
/// the standard idiom and depends on the build-time expansion this gets from Docker. Quotes and
/// backslashes are escaped so a value cannot terminate its own instruction.
fn container_env_instructions(feature: &ResolvedFeature) -> String {
    feature
        .raw
        .get("containerEnv")
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| value.as_str().map(|v| (key, v)))
                .map(|(key, value)| {
                    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
                    format!("ENV {key}=\"{escaped}\"\n")
                })
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// Generate the Dockerfile that installs the Features and stamps the metadata label.
pub fn render(
    features: &[ResolvedFeature],
    label: &str,
    container_user: &str,
    remote_user: &str,
) -> String {
    let mut out = String::new();
    out.push_str("ARG _DEV_CONTAINERS_BASE_IMAGE=placeholder\n\n");
    out.push_str("FROM $_DEV_CONTAINERS_BASE_IMAGE AS dev_containers_target_stage\n\n");
    out.push_str("USER root\n\n");

    if !features.is_empty() {
        out.push_str(&format!("RUN mkdir -p {FEATURES_DIR}\n"));
        out.push_str(&format!("COPY --chown=root:root . {FEATURES_DIR}\n\n"));

        // Resolved inside the image because only the image knows its own /etc/passwd.
        out.push_str(&format!(
            "RUN \\\n{}\\\n{}\n\n",
            home_probe("_CONTAINER_USER_HOME", container_user),
            home_probe("_REMOTE_USER_HOME", remote_user),
        ));

        for (index, feature) in features.iter().enumerate() {
            let dir = feature.dir_name(index);
            out.push_str(&format!("# {}\n", feature.reference.raw));
            // A Feature's `containerEnv` is baked in as `ENV`, before its own install step so
            // that later Features see it too. This is the whole toolchain contract for `go`,
            // `node`, `python`, `rust` and friends — they install into a prefix and put it on
            // `PATH` here — and it is deliberately *absent* from the Feature's label snippet,
            // so nothing downstream would ever restore it.
            out.push_str(&container_env_instructions(feature));
            out.push_str(&format!(
                "RUN chmod -R 0755 {FEATURES_DIR}/{dir} \\\n\
                 && cd {FEATURES_DIR}/{dir} \\\n\
                 && chmod +x ./devcontainer-features-install.sh \\\n\
                 && ./devcontainer-features-install.sh\n\n"
            ));
        }
    }

    out.push_str("ARG _DEV_CONTAINERS_IMAGE_USER=root\n");
    out.push_str("USER $_DEV_CONTAINERS_IMAGE_USER\n\n");
    out.push_str(&format!(
        "LABEL devcontainer.metadata=\"{}\"\n",
        escape_label(label)
    ));
    out
}

/// The `getent`-with-`/etc/passwd`-fallback probe the install contract expects.
fn home_probe(var: &str, user: &str) -> String {
    format!(
        "echo \"{var}=$( (command -v getent >/dev/null 2>&1 && getent passwd '{user}' || \
         grep -E '^{user}|^[^:]*:[^:]*:{user}:' /etc/passwd || true) | cut -d: -f6)\" \
         >> {FEATURES_DIR}/devcontainer-features.builtin.env "
    )
}

/// Escape a metadata label for embedding in a Dockerfile `LABEL` instruction.
///
/// `$` must be escaped along with the quoting: Dockerfile instructions undergo variable
/// substitution, and an unescaped `${localWorkspaceFolder}` in a Feature's mount would expand
/// to the empty string at build time — destroying a value the run path is required to
/// substitute itself, later, with the session's real paths.
fn escape_label(label: &str) -> String {
    label
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('$', "\\$")
        .replace('\n', " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devcontainer::native::feature::{parse_metadata, resolve_options, ResolvedFeature};
    use crate::devcontainer::native::oci;
    use std::collections::BTreeMap;

    fn git_feature() -> ResolvedFeature {
        let text =
            include_str!("../../../tests/fixtures/devcontainer/native/git-devcontainer-feature.json");
        let (metadata, raw) = parse_metadata(text).unwrap();
        let options = resolve_options(&metadata, &BTreeMap::new());
        let reference = oci::parse_ref("ghcr.io/devcontainers/features/git:1");
        ResolvedFeature {
            reference,
            metadata,
            raw,
            options,
            supplied: BTreeMap::new(),
            digest: "sha256:test".to_string(),
            hard_deps: Vec::new(),
        }
    }

    #[test]
    fn options_env_matches_the_cli_output() {
        let expected =
            include_str!("../../../tests/fixtures/devcontainer/native/cli-git_0-features.env");
        let generated = options_env(&git_feature());
        // Compare as sets of lines: the CLI emits declaration order, am emits sorted order,
        // and `set -a` sourcing makes the order irrelevant.
        let mut want: Vec<&str> = expected.lines().filter(|l| !l.is_empty()).collect();
        let mut got: Vec<&str> = generated.lines().filter(|l| !l.is_empty()).collect();
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want);
    }

    /// The same comparison for a Feature whose option names need normalising.
    ///
    /// `cli-git_0-features.env` cannot catch a normalisation bug — `version` and `ppa` survive
    /// naive uppercasing intact — and neither can the label differential tests, since the label
    /// carries option *values* contributed by the config, never the Feature's option names. This
    /// fixture is the only thing standing between a rename rule and `MY-OPTION="a"`, which is
    /// not a valid shell assignment and fails the whole install when the env file is sourced.
    #[test]
    fn awkward_option_names_match_the_cli_output() {
        let text = include_str!(
            "../../../tests/fixtures/devcontainer/native/awkward-devcontainer-feature.json"
        );
        let (metadata, raw) = parse_metadata(text).unwrap();
        let options = resolve_options(&metadata, &BTreeMap::new());
        let feature = ResolvedFeature {
            reference: oci::parse_ref("./awkward"),
            metadata,
            raw,
            options,
            supplied: BTreeMap::new(),
            digest: "local:test".to_string(),
            hard_deps: Vec::new(),
        };

        let expected =
            include_str!("../../../tests/fixtures/devcontainer/native/cli-awkward-features.env");
        let generated = options_env(&feature);
        // Sets of lines, same reason as above.
        let mut want: Vec<&str> = expected.lines().filter(|l| !l.is_empty()).collect();
        let mut got: Vec<&str> = generated.lines().filter(|l| !l.is_empty()).collect();
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, want);
    }

    #[test]
    fn install_wrapper_sources_both_env_files_in_contract_order() {
        let wrapper = install_wrapper(&git_feature());
        let builtin = wrapper.find(". ../devcontainer-features.builtin.env").unwrap();
        let own = wrapper.find(". ./devcontainer-features.env").unwrap();
        // Feature options must win over the builtin values, so they are sourced second.
        assert!(builtin < own);
        assert!(wrapper.contains("set -a"));
        assert!(wrapper.contains("./install.sh"));
    }

    #[test]
    fn dockerfile_installs_features_in_the_given_order() {
        let mut second = git_feature();
        second.reference.raw = "ghcr.io/devcontainers/features/node:1".to_string();
        second.reference.repository = "devcontainers/features/node".to_string();
        // The declared id is what names the staging directory, not the reference.
        second.metadata.id = Some("node".to_string());
        let rendered = render(&[git_feature(), second], "[]", "root", "root");

        let git = rendered.find("dev-container-features/git_0").unwrap();
        let node = rendered.find("dev-container-features/node_1").unwrap();
        assert!(git < node, "index order must follow install order");
    }

    #[test]
    fn a_features_container_env_becomes_env_before_its_install_step() {
        // The toolchain contract for go, node, python, rust and friends: they install into a
        // prefix and put it on PATH through containerEnv. It is deliberately absent from the
        // Feature's label snippet, so if the Dockerfile does not carry it nothing downstream
        // ever will — the Feature installs and its tools are not on PATH.
        let mut feature = git_feature();
        feature.raw = serde_json_lenient::from_str(
            r#"{"containerEnv":{"GOROOT":"/usr/local/go","PATH":"/usr/local/go/bin:${PATH}"}}"#,
        )
        .unwrap();
        let rendered = render(&[feature], "[]", "root", "root");

        assert!(rendered.contains(r#"ENV GOROOT="/usr/local/go""#), "got: {rendered}");
        // `${PATH}` is left unescaped on purpose: the standard idiom depends on Docker
        // expanding it at build time, unlike the metadata label where `$` must survive.
        assert!(rendered.contains(r#"ENV PATH="/usr/local/go/bin:${PATH}""#), "got: {rendered}");

        let env_at = rendered.find("ENV GOROOT").unwrap();
        let install_at = rendered.find("devcontainer-features-install.sh").unwrap();
        assert!(env_at < install_at, "ENV must precede the install step so later Features see it");
    }

    #[test]
    fn a_feature_with_no_container_env_adds_no_env_instructions() {
        let rendered = render(&[git_feature()], "[]", "root", "root");
        assert!(!rendered.contains("\nENV "), "got: {rendered}");
    }

    #[test]
    fn a_container_env_value_cannot_terminate_its_own_instruction() {
        let mut feature = git_feature();
        // JSON escapes: Q's value is  a"b  and B's is  c\d
        feature.raw =
            serde_json_lenient::from_str(r#"{"containerEnv":{"Q":"a\"b","B":"c\\d"}}"#).unwrap();
        let rendered = render(&[feature], "[]", "root", "root");
        assert!(rendered.contains(r#"ENV Q="a\"b""#), "got: {rendered}");
        assert!(rendered.contains(r#"ENV B="c\\d""#), "got: {rendered}");
    }

    #[test]
    fn dockerfile_without_features_still_stamps_the_label() {
        let rendered = render(&[], r#"[ {"remoteUser":"root"} ]"#, "root", "root");
        assert!(rendered.contains("LABEL devcontainer.metadata="));
        // Nothing to copy, so the features scaffolding must not appear at all.
        assert!(!rendered.contains("COPY"));
        assert!(!rendered.contains("mkdir -p /tmp/dev-container-features"));
    }

    #[test]
    fn label_quotes_are_escaped() {
        let escaped = escape_label(r#"[ {"remoteUser":"root"} ]"#);
        assert_eq!(escaped, r#"[ {\"remoteUser\":\"root\"} ]"#);
    }

    #[test]
    fn label_dollar_signs_are_escaped_so_docker_does_not_expand_them() {
        // This is the property the run path depends on: am does its own substitution against
        // the session's worktree, so the variable has to survive the build verbatim.
        let escaped = escape_label(r#"{"source":"${localWorkspaceFolder}"}"#);
        assert!(escaped.contains(r"\$\{localWorkspaceFolder\}") || escaped.contains(r"\${localWorkspaceFolder}"));
        assert!(!escaped.contains("\"${localWorkspaceFolder}\""));
    }

    #[test]
    fn label_newlines_are_flattened() {
        // A raw newline would terminate the LABEL instruction mid-value.
        assert!(!escape_label("[\n{}\n]").contains('\n'));
    }

    #[test]
    fn option_values_containing_quotes_are_escaped() {
        let mut feature = git_feature();
        feature
            .options
            .insert("MSG".to_string(), r#"say "hi""#.to_string());
        let env = options_env(&feature);
        assert!(env.contains(r#"MSG="say \"hi\""."#) || env.contains(r#"MSG="say \"hi\"""#));
    }

    #[test]
    fn context_layout_matches_the_install_contract() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("cached");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("install.sh"), "echo hi").unwrap();

        let context = tmp.path().join("ctx");
        write_context(
            &context,
            &[git_feature()],
            &[source],
            "root",
            "vscode",
        )
        .unwrap();

        let builtin =
            std::fs::read_to_string(context.join("devcontainer-features.builtin.env")).unwrap();
        assert!(builtin.contains("_CONTAINER_USER=root"));
        assert!(builtin.contains("_REMOTE_USER=vscode"));

        let dir = context.join("git_0");
        assert!(dir.join("install.sh").is_file(), "cached files are staged");
        assert!(dir.join("devcontainer-features.env").is_file());
        assert!(dir.join("devcontainer-features-install.sh").is_file());
    }
}
