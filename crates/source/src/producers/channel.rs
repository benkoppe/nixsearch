use std::collections::BTreeMap;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use tokio::process::Command;

use nixsearch_core::ArtifactKind;
use nixsearch_store::{ArtifactMetadataInput, ArtifactStore};

use crate::artifact::{ProduceRequest, ProducedArtifact};

use super::Producer;
use super::download::{fetch_brotli_artifact, fetch_text};

const CHANNELS_BASE_URL: &str = "https://channels.nixos.org";
const CHANNEL_PACKAGES_FILE: &str = "packages.json.br";
const CHANNEL_OPTIONS_FILE: &str = "options.json.br";
const CHANNEL_GIT_REVISION_FILE: &str = "git-revision";

#[derive(Debug, Clone)]
pub struct ChannelPackagesJsonProducer {
    channel: String,
    url: Option<String>,
    producer_name: String,
}

impl ChannelPackagesJsonProducer {
    pub fn new(channel: impl Into<String>, url: Option<String>) -> Self {
        Self {
            channel: channel.into(),
            url,
            producer_name: "channel-packages-json".to_owned(),
        }
    }

    fn url(&self) -> String {
        self.url
            .clone()
            .unwrap_or_else(|| channel_artifact_url(&self.channel, CHANNEL_PACKAGES_FILE))
    }
}

#[async_trait]
impl Producer for ChannelPackagesJsonProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let url = self.url();
        let mut bytes = fetch_brotli_artifact(&url)
            .await
            .with_context(|| format!("failed to fetch channel packages artifact from {url}"))?;

        let artifact_ref = request.artifact_ref(ArtifactKind::PackagesJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(url.clone());
        populate_channel_revision(&mut metadata_input, &self.channel).await;

        enrich_channel_packages_with_programs(&mut bytes, &self.channel, &mut metadata_input).await;

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write packages artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

#[derive(Debug, Clone)]
pub struct ChannelOptionsJsonProducer {
    channel: String,
    url: Option<String>,
    producer_name: String,
}

impl ChannelOptionsJsonProducer {
    pub fn new(channel: impl Into<String>, url: Option<String>) -> Self {
        Self {
            channel: channel.into(),
            url,
            producer_name: "channel-options-json".to_owned(),
        }
    }

    fn url(&self) -> String {
        self.url
            .clone()
            .unwrap_or_else(|| channel_artifact_url(&self.channel, CHANNEL_OPTIONS_FILE))
    }
}

#[async_trait]
impl Producer for ChannelOptionsJsonProducer {
    async fn produce(
        &self,
        store: &ArtifactStore,
        request: &ProduceRequest,
    ) -> Result<ProducedArtifact> {
        let url = self.url();
        let bytes = fetch_brotli_artifact(&url)
            .await
            .with_context(|| format!("failed to fetch channel options artifact from {url}"))?;

        let artifact_ref = request.artifact_ref(ArtifactKind::OptionsJson);

        let mut metadata_input = ArtifactMetadataInput::new(self.producer_name.clone());
        metadata_input.source_url = Some(url.clone());
        populate_channel_revision(&mut metadata_input, &self.channel).await;

        let metadata = store
            .put_artifact(&artifact_ref, Bytes::from(bytes), metadata_input)
            .await
            .context("failed to write options artifact to store")?;

        Ok(ProducedArtifact {
            artifact_ref,
            metadata,
        })
    }
}

fn channel_artifact_url(channel: &str, file_name: &str) -> String {
    format!("{CHANNELS_BASE_URL}/{channel}/{file_name}")
}

async fn enrich_channel_packages_with_programs(
    bytes: &mut Vec<u8>,
    channel: &str,
    metadata_input: &mut ArtifactMetadataInput,
) {
    match channel_package_programs(channel).await {
        Ok(programs) => {
            if let Err(error) = merge_package_programs(bytes, programs) {
                metadata_input.warnings.push(format!(
                    "failed to merge programs.sqlite data into packages artifact: {error:#}"
                ));
            }
        }
        Err(error) => metadata_input.warnings.push(format!(
            "failed to read programs.sqlite for channel {channel:?}: {error:#}"
        )),
    }
}

async fn channel_package_programs(channel: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let output = Command::new("nix-instantiate")
        .arg("--eval")
        .arg("--json")
        .arg("-I")
        .arg(format!("nixpkgs=channel:{channel}"))
        .arg("--expr")
        .arg("toString <nixpkgs/programs.sqlite>")
        .output()
        .await
        .with_context(|| format!("failed to run nix-instantiate for channel {channel:?}"))?;

    if !output.status.success() {
        anyhow::bail!(
            "nix-instantiate failed with status {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let db_path: String = serde_json::from_slice(&output.stdout)
        .context("failed to parse programs.sqlite path from nix-instantiate output")?;

    read_programs_sqlite(&db_path)
}

fn read_programs_sqlite(path: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let connection = rusqlite::Connection::open(path)
        .with_context(|| format!("failed to open programs.sqlite at {path:?}"))?;
    let mut statement = connection
        .prepare("SELECT name, package FROM Programs")
        .context("failed to prepare programs.sqlite query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("failed to query programs.sqlite")?;

    let mut programs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (name, package) = row.context("failed to read programs.sqlite row")?;
        programs.entry(package).or_default().push(name);
    }

    for names in programs.values_mut() {
        names.sort();
        names.dedup();
    }

    Ok(programs)
}

fn merge_package_programs(
    bytes: &mut Vec<u8>,
    programs: BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let mut value: serde_json::Value =
        serde_json::from_slice(bytes).context("failed to parse packages JSON")?;
    let packages = value
        .get_mut("packages")
        .and_then(serde_json::Value::as_object_mut)
        .context("packages JSON does not contain a packages object")?;

    for (attribute, names) in programs {
        if let Some(package) = packages
            .get_mut(&attribute)
            .and_then(serde_json::Value::as_object_mut)
        {
            package.insert(
                "programs".to_owned(),
                serde_json::Value::Array(
                    names.into_iter().map(serde_json::Value::String).collect(),
                ),
            );
        }
    }

    *bytes = serde_json::to_vec(&value).context("failed to serialize packages JSON")?;

    Ok(())
}

async fn populate_channel_revision(metadata_input: &mut ArtifactMetadataInput, channel: &str) {
    match fetch_channel_git_revision(channel).await {
        Ok(Some(revision)) => metadata_input.revision = Some(revision),
        Ok(None) => metadata_input.warnings.push(format!(
            "channel git-revision was empty for channel {channel:?}"
        )),
        Err(error) => metadata_input.warnings.push(format!(
            "failed to fetch channel git-revision for channel {channel:?}: {error:#}"
        )),
    }
}

fn channel_git_revision_url(channel: &str) -> String {
    channel_artifact_url(channel, CHANNEL_GIT_REVISION_FILE)
}

async fn fetch_channel_git_revision(channel: &str) -> Result<Option<String>> {
    let url = channel_git_revision_url(channel);
    let text = fetch_text(&url).await?;
    let revision = text.trim();

    if revision.is_empty() {
        Ok(None)
    } else {
        Ok(Some(revision.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::{ChannelOptionsJsonProducer, ChannelPackagesJsonProducer};

    use super::merge_package_programs;

    #[test]
    fn channel_packages_json_producer_builds_default_url() {
        let producer = ChannelPackagesJsonProducer::new("nixos-unstable", None);

        assert_eq!(
            producer.url(),
            "https://channels.nixos.org/nixos-unstable/packages.json.br"
        );
    }

    #[test]
    fn merge_package_programs_adds_program_lists_by_attribute() {
        let mut bytes = br#"{
            "version": "2",
            "packages": {
                "git": { "pname": "git", "meta": { "mainProgram": "git" } },
                "ripgrep": { "pname": "ripgrep", "meta": { "mainProgram": "rg" } }
            }
        }"#
        .to_vec();
        let programs = BTreeMap::from([
            (
                "git".to_owned(),
                vec![
                    "git".to_owned(),
                    "git-shell".to_owned(),
                    "scalar".to_owned(),
                ],
            ),
            ("missing".to_owned(), vec!["missing-bin".to_owned()]),
        ]);

        merge_package_programs(&mut bytes, programs).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            value["packages"]["git"]["programs"],
            serde_json::json!(["git", "git-shell", "scalar"])
        );
        assert!(value["packages"]["ripgrep"].get("programs").is_none());
    }

    #[test]
    fn channel_options_json_producer_builds_default_url() {
        let producer = ChannelOptionsJsonProducer::new("nixos-unstable", None);

        assert_eq!(
            producer.url(),
            "https://channels.nixos.org/nixos-unstable/options.json.br"
        );
    }

    #[test]
    fn channel_git_revision_url_uses_channel() {
        assert_eq!(
            super::channel_git_revision_url("nixos-unstable"),
            "https://channels.nixos.org/nixos-unstable/git-revision"
        );
    }
}
