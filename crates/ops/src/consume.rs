use anyhow::{Context, Result, bail};

use nixsearch_config::source::SourceKind;
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::SearchDocument;
use nixsearch_source::artifact::ProducedArtifact;
use nixsearch_source::consumer::{
    Consumer, FlakeInfoJsonConsumer, OptionsJsonConsumer, PackagesJsonConsumer,
};
use nixsearch_store::ArtifactStore;

use crate::targets::TargetRef;
use crate::targets::latest_artifact_ref_for_target;

pub async fn consume_target(
    store: &ArtifactStore,
    target: &TargetRef,
    produced: &ProducedArtifact,
) -> Result<Vec<SearchDocument>> {
    let expected_ref = latest_artifact_ref_for_target(target);
    if produced.artifact_ref != expected_ref {
        bail!(
            "artifact ref mismatch for {}/{}: expected {:?}, got {:?}",
            target.source_id,
            target.ref_config.id,
            expected_ref,
            produced.artifact_ref
        );
    }

    if !target
        .source_kind
        .accepts_artifact_kind(produced.artifact_ref.kind)
    {
        bail!(
            "no consumer implemented for source kind {:?} and artifact kind {:?}",
            target.source_kind,
            produced.artifact_ref.kind
        );
    }

    match (target.source_kind, produced.artifact_ref.kind) {
        (SourceKind::Options | SourceKind::Mixed, ArtifactKind::OptionsJson) => {
            let consumer = OptionsJsonConsumer::new(target.strip_prefixes.clone());

            consumer.consume(store, produced).await.with_context(|| {
                format!(
                    "failed to consume options artifact for {}/{}",
                    target.source_id, target.ref_config.id
                )
            })
        }

        (SourceKind::Packages | SourceKind::Mixed, ArtifactKind::PackagesJson) => {
            let consumer = PackagesJsonConsumer::new(target.strip_prefixes.clone());

            consumer.consume(store, produced).await.with_context(|| {
                format!(
                    "failed to consume packages artifact for {}/{}",
                    target.source_id, target.ref_config.id
                )
            })
        }

        (
            SourceKind::Apps | SourceKind::Services | SourceKind::Mixed,
            ArtifactKind::FlakeInfoJson,
        ) => {
            let consumer = FlakeInfoJsonConsumer;

            consumer.consume(store, produced).await.with_context(|| {
                format!(
                    "failed to consume flake-info artifact for {}/{}",
                    target.source_id, target.ref_config.id
                )
            })
        }

        (kind, artifact) => bail!(
            "no consumer implemented for source kind {:?} and artifact kind {:?}",
            kind,
            artifact
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bytes::Bytes;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    use nixsearch_config::producer::ProducerConfig;
    use nixsearch_config::source::{RefConfig, SourceKind};
    use nixsearch_core::target::RefRole;
    use nixsearch_store::{ArtifactMetadata, ArtifactMetadataInput, ArtifactRef, ArtifactStore};

    use super::*;

    const SOURCE: &str = "fixtures";
    const REF: &str = "small";

    fn target(source_kind: SourceKind, artifact_kind: ArtifactKind) -> TargetRef {
        TargetRef {
            source_id: SOURCE.to_owned(),
            source_kind,
            strip_prefixes: Vec::new(),
            ref_config: RefConfig {
                id: REF.to_owned(),
                role: if artifact_kind == ArtifactKind::FlakeInfoJson {
                    RefRole::ArtifactOnly
                } else {
                    RefRole::Search
                },
                producer: ProducerConfig::ExistingFile {
                    path: PathBuf::from("unused.json"),
                    artifact: artifact_kind,
                },
                source_links: None,
            },
        }
    }

    async fn put_artifact(
        store: &ArtifactStore,
        artifact_kind: ArtifactKind,
        bytes: Bytes,
    ) -> ProducedArtifact {
        let artifact_ref = ArtifactRef::latest(SOURCE, REF, artifact_kind);
        let metadata = store
            .put_artifact(
                &artifact_ref,
                bytes,
                ArtifactMetadataInput::new("test-producer"),
            )
            .await
            .unwrap();

        ProducedArtifact {
            artifact_ref,
            metadata,
        }
    }

    fn missing_produced(artifact_kind: ArtifactKind) -> ProducedArtifact {
        let artifact_ref = ArtifactRef::latest(SOURCE, REF, artifact_kind);
        let metadata = ArtifactMetadata {
            source: SOURCE.to_owned(),
            ref_id: REF.to_owned(),
            kind: artifact_kind,
            producer: "test-producer".to_owned(),
            revision: None,
            source_url: None,
            content_hash: "missing".to_owned(),
            size_bytes: 0,
            produced_at: OffsetDateTime::now_utc(),
            warnings: Vec::new(),
        };

        ProducedArtifact {
            artifact_ref,
            metadata,
        }
    }

    #[tokio::test]
    async fn flake_info_json_consumes_to_zero_documents_for_apps_services_and_mixed() {
        for source_kind in [SourceKind::Apps, SourceKind::Services, SourceKind::Mixed] {
            let tempdir = tempdir().unwrap();
            let store = ArtifactStore::local(tempdir.path()).unwrap();
            let target = target(source_kind, ArtifactKind::FlakeInfoJson);
            let produced = put_artifact(
                &store,
                ArtifactKind::FlakeInfoJson,
                Bytes::from_static(br#"[{"type":"app","app_attr_name":"hello"}]"#),
            )
            .await;

            let documents = consume_target(&store, &target, &produced).await.unwrap();

            assert!(documents.is_empty());
        }
    }

    #[tokio::test]
    async fn flake_info_json_invalid_json_errors() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let target = target(SourceKind::Apps, ArtifactKind::FlakeInfoJson);
        let produced = put_artifact(
            &store,
            ArtifactKind::FlakeInfoJson,
            Bytes::from_static(b"not json"),
        )
        .await;

        let error = consume_target(&store, &target, &produced)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("failed to parse flake-info artifact as JSON"));
    }

    #[tokio::test]
    async fn flake_info_json_invalid_shape_errors_with_target_context() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let target = target(SourceKind::Apps, ArtifactKind::FlakeInfoJson);
        let produced = put_artifact(
            &store,
            ArtifactKind::FlakeInfoJson,
            Bytes::from_static(br#"{"items":[]}"#),
        )
        .await;

        let error = consume_target(&store, &target, &produced)
            .await
            .unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("failed to consume flake-info artifact for fixtures/small"));
        assert!(message.contains("invalid flake-info artifact shape"));
    }

    #[tokio::test]
    async fn flake_info_json_missing_artifact_bytes_errors() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let target = target(SourceKind::Apps, ArtifactKind::FlakeInfoJson);
        let produced = missing_produced(ArtifactKind::FlakeInfoJson);

        let error = consume_target(&store, &target, &produced)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("failed to read verified flake-info artifact"));
    }

    #[tokio::test]
    async fn unsupported_source_artifact_combinations_still_error() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let target = target(SourceKind::Options, ArtifactKind::FlakeInfoJson);
        let produced = put_artifact(
            &store,
            ArtifactKind::FlakeInfoJson,
            Bytes::from_static(br#"[{"type":"app","app_attr_name":"hello"}]"#),
        )
        .await;

        let error = consume_target(&store, &target, &produced)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("no consumer implemented"));
    }

    #[tokio::test]
    async fn mixed_options_json_route_still_consumes_documents() {
        let tempdir = tempdir().unwrap();
        let store = ArtifactStore::local(tempdir.path()).unwrap();
        let target = target(SourceKind::Mixed, ArtifactKind::OptionsJson);
        let produced = put_artifact(
            &store,
            ArtifactKind::OptionsJson,
            Bytes::from_static(include_bytes!(
                "../../../fixtures/search-small/options.json"
            )),
        )
        .await;

        let documents = consume_target(&store, &target, &produced).await.unwrap();

        assert_eq!(documents.len(), 4);
    }
}
