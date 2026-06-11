use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::{TempDir, tempdir};
use tokio::process::Command;

use crate::error::NixCommandFailure;

pub(crate) fn create_tempdir(label: &str) -> Result<TempDir> {
    let temp_parent = std::env::temp_dir();

    std::fs::create_dir_all(&temp_parent).with_context(|| {
        format!(
            "failed to create temporary {label} parent directory {}",
            temp_parent.display()
        )
    })?;

    tempdir().with_context(|| format!("failed to create temporary {label} directory"))
}

pub(crate) async fn run_nix_build_installable(installable: &str) -> Result<PathBuf> {
    run_nix_build_installable_with_overrides(installable, &BTreeMap::new()).await
}

pub(crate) async fn run_nix_build_installable_with_overrides(
    installable: &str,
    input_overrides: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    let mut command = Command::new("nix");
    command.args(nix_build_installable_args(installable, input_overrides));

    let output = command
        .output()
        .await
        .with_context(|| format!("failed to run nix build for installable {installable:?}"))?;

    if !output.status.success() {
        return Err(NixCommandFailure {
            command: "nix build",
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        }
        .into());
    }

    let stdout = String::from_utf8(output.stdout).context("nix stdout was not valid UTF-8")?;

    let output_path = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .context("nix build succeeded but did not print an output path")?;

    Ok(PathBuf::from(output_path))
}

fn nix_build_installable_args(
    installable: &str,
    input_overrides: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut args = vec![
        "build".to_owned(),
        "--extra-experimental-features".to_owned(),
        "nix-command flakes".to_owned(),
        "--no-link".to_owned(),
        "--print-out-paths".to_owned(),
    ];

    for (name, source_ref) in input_overrides {
        args.push("--override-input".to_owned());
        args.push(name.clone());
        args.push(source_ref.clone());
    }

    args.push(installable.to_owned());
    args
}

pub(crate) async fn run_nix_build_expression(expression_path: &Path) -> Result<PathBuf> {
    let output = Command::new("nix")
        .arg("build")
        .arg("--extra-experimental-features")
        .arg("nix-command flakes")
        .arg("--impure")
        .arg("--no-link")
        .arg("--print-out-paths")
        .arg("--file")
        .arg(expression_path)
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to run nix build for expression {}",
                expression_path.display()
            )
        })?;

    if !output.status.success() {
        return Err(NixCommandFailure {
            command: "nix build",
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        }
        .into());
    }

    let stdout = String::from_utf8(output.stdout).context("nix stdout was not valid UTF-8")?;

    let output_path = stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .context("nix build succeeded but did not print an output path")?;

    Ok(PathBuf::from(output_path))
}

pub(crate) fn normalize_nix_path_source(source_ref: &str) -> String {
    if let Some(rest) = source_ref.strip_prefix("github:") {
        let mut parts = rest.splitn(3, '/');

        if let (Some(owner), Some(repo), Some(branch_or_ref)) =
            (parts.next(), parts.next(), parts.next())
        {
            return format!(
                "https://github.com/{owner}/{repo}/archive/refs/heads/{branch_or_ref}.tar.gz"
            );
        }
    }

    source_ref.to_owned()
}

pub(crate) fn normalize_flake_ref(source_ref: &str) -> Result<String> {
    let Some(path) = source_ref.strip_prefix("path:") else {
        return Ok(source_ref.to_owned());
    };

    let path = PathBuf::from(path);

    if path.is_absolute() {
        return Ok(source_ref.to_owned());
    }

    let absolute = std::env::current_dir()
        .context("failed to get current directory while normalizing relative path flake ref")?
        .join(path)
        .canonicalize()
        .with_context(|| format!("failed to canonicalize relative flake ref {source_ref:?}"))?;

    Ok(format!("path:{}", absolute.display()))
}

pub(crate) async fn resolve_flake_revision(source_ref: &str) -> Result<Option<String>> {
    let output = Command::new("nix")
        .arg("--extra-experimental-features")
        .arg("nix-command flakes")
        .arg("flake")
        .arg("metadata")
        .arg("--json")
        .arg(source_ref)
        .output()
        .await
        .with_context(|| format!("failed to run nix flake metadata for {source_ref:?}"))?;

    if !output.status.success() {
        anyhow::bail!(
            "nix flake metadata failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    parse_flake_metadata_revision(&output.stdout)
}

fn parse_flake_metadata_revision(bytes: &[u8]) -> Result<Option<String>> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).context("failed to parse nix flake metadata JSON")?;

    Ok(value
        .get("locked")
        .and_then(|locked| locked.get("rev"))
        .and_then(|rev| rev.as_str())
        .filter(|rev| !rev.trim().is_empty())
        .map(ToOwned::to_owned))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use tempfile::tempdir;

    #[test]
    fn normalizes_github_flake_ref_to_tarball_url() {
        let normalized = super::normalize_nix_path_source("github:NixOS/nixpkgs/nixos-unstable");

        assert_eq!(
            normalized,
            "https://github.com/NixOS/nixpkgs/archive/refs/heads/nixos-unstable.tar.gz"
        );
    }

    #[test]
    fn leaves_non_github_sources_unchanged() {
        let source = "https://channels.nixos.org/nixos-unstable/nixexprs.tar.xz";

        assert_eq!(super::normalize_nix_path_source(source), source);
    }

    #[test]
    fn normalize_flake_ref_canonicalizes_relative_path_refs() {
        let tempdir = tempdir().unwrap();
        let flake_dir = tempdir.path().join("flake");
        fs::create_dir(&flake_dir).unwrap();

        let previous_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tempdir.path()).unwrap();

        let normalized = super::normalize_flake_ref("path:./flake").unwrap();

        std::env::set_current_dir(previous_dir).unwrap();

        assert_eq!(
            normalized,
            format!("path:{}", flake_dir.canonicalize().unwrap().display())
        );
    }

    #[test]
    fn normalize_flake_ref_leaves_non_path_refs_unchanged() {
        let source_ref = "github:NixOS/nixpkgs/nixos-unstable";

        let normalized = super::normalize_flake_ref(source_ref).unwrap();

        assert_eq!(normalized, source_ref);
    }

    #[test]
    fn nix_build_installable_args_without_overrides() {
        assert_eq!(
            super::nix_build_installable_args("github:example/project#docs", &BTreeMap::new()),
            [
                "build",
                "--extra-experimental-features",
                "nix-command flakes",
                "--no-link",
                "--print-out-paths",
                "github:example/project#docs",
            ]
        );
    }

    #[test]
    fn nix_build_installable_args_with_overrides() {
        let input_overrides = BTreeMap::from([(
            "nixpkgs".to_owned(),
            "github:NixOS/nixpkgs/nixpkgs-unstable".to_owned(),
        )]);

        assert_eq!(
            super::nix_build_installable_args("github:example/project#docs", &input_overrides),
            [
                "build",
                "--extra-experimental-features",
                "nix-command flakes",
                "--no-link",
                "--print-out-paths",
                "--override-input",
                "nixpkgs",
                "github:NixOS/nixpkgs/nixpkgs-unstable",
                "github:example/project#docs",
            ]
        );
    }

    #[test]
    fn parse_flake_metadata_revision_reads_locked_rev() {
        let json = br#"
          {
            "locked": {
              "rev": "abc123"
            }
          }
          "#;

        let revision = super::parse_flake_metadata_revision(json).unwrap();

        assert_eq!(revision.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_flake_metadata_revision_returns_none_when_missing() {
        let json = br#"
          {
            "locked": {}
          }
          "#;

        let revision = super::parse_flake_metadata_revision(json).unwrap();

        assert_eq!(revision, None);
    }

    #[test]
    fn parse_flake_metadata_revision_returns_none_for_path_flake_metadata() {
        let json = br#"
          {
            "path": "/tmp/flake",
            "resolved": {
              "type": "path",
              "path": "/tmp/flake"
            }
          }
          "#;

        let revision = super::parse_flake_metadata_revision(json).unwrap();

        assert_eq!(revision, None);
    }

    #[test]
    fn parse_flake_metadata_revision_rejects_malformed_json() {
        let error = super::parse_flake_metadata_revision(b"{ not json").unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to parse nix flake metadata JSON")
        );
    }

    #[tokio::test]
    #[ignore = "requires nix and network"]
    async fn resolves_github_flake_revision() {
        let revision = super::resolve_flake_revision("github:NixOS/nixpkgs/nixos-unstable")
            .await
            .unwrap();

        let revision = revision.expect("expected github flake to resolve to a revision");

        assert!(!revision.is_empty());
    }
}
