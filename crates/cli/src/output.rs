use nixsearch_config::app::AppConfig;
use nixsearch_config::source::SourceConfig;
use nixsearch_core::document::{CommonDoc, SearchDocument};
use nixsearch_core::source_link::{SourceLinkConfig, SourceLinkResolver};
use nixsearch_index::search::SearchHit;
use nixsearch_source::artifact::ProducedArtifact;
use nixsearch_store::ArtifactMetadata;

pub(crate) fn print_source(source_id: &str, source: &SourceConfig) {
    let name = source.name.as_deref().unwrap_or(source_id);
    println!("  source {source_id}: {name} ({:?})", source.kind);

    for ref_config in &source.refs {
        println!(
            "    ref {}: producer={:?}",
            ref_config.id,
            ref_config.producer.kind()
        );
    }
}

pub(crate) fn print_produced_artifact(produced: &ProducedArtifact) {
    println!("produced artifact");
    println!("  source = {}", produced.metadata.source);
    println!("  ref = {}", produced.metadata.ref_id);
    println!("  kind = {:?}", produced.metadata.kind);
    println!("  producer = {}", produced.metadata.producer);
    println!(
        "  revision = {}",
        produced.metadata.revision.as_deref().unwrap_or("-")
    );
    println!(
        "  upstream = {}",
        produced.metadata.source_url.as_deref().unwrap_or("-")
    );
    println!("  hash = {}", produced.metadata.content_hash);
    println!("  size = {}", produced.metadata.size_bytes);

    for warning in &produced.metadata.warnings {
        println!("warning = {warning}");
    }
}

pub(crate) fn print_artifact_metadata(metadata: &ArtifactMetadata) {
    println!("artifact");
    println!("  source = {}", metadata.source);
    println!("  ref = {}", metadata.ref_id);
    println!("  kind = {:?}", metadata.kind);
    println!("  producer = {}", metadata.producer);
    println!(
        "  revision = {}",
        metadata.revision.as_deref().unwrap_or("-")
    );
    println!(
        "  upstream = {}",
        metadata.source_url.as_deref().unwrap_or("-")
    );
    println!("  hash = {}", metadata.content_hash);
    println!("  size = {}", metadata.size_bytes);
    println!("  produced_at = {}", metadata.produced_at);

    for warning in &metadata.warnings {
        println!("  warning = {warning}");
    }
}

pub(crate) fn print_search_hit(config: &AppConfig, hit: SearchHit) {
    let common = hit.document.common().clone();

    println!(
        "{score:.3}  {kind}  {source}/{ref_id}  {name}",
        score = hit.score,
        kind = common.kind.as_str(),
        source = common.source,
        ref_id = common.ref_id,
        name = common.name,
    );

    if !common.name_parts.groups.is_empty() {
        println!("       groups: {}", common.name_parts.groups.join(", "));
    }

    let resolver = source_link_config_for_document(config, &common)
        .map(|source_links| SourceLinkResolver::new(source_links, common.revision.as_deref()));

    match hit.document {
        SearchDocument::Option(option) => {
            if let Some(description) = option.description {
                let description = description.plain_text();
                let summary = description.lines().next().unwrap_or("").trim();

                if !summary.is_empty() {
                    println!("       {summary}");
                }
            }

            if let Some(resolver) = resolver {
                for declaration in &option.declarations {
                    if let Some(url) = resolver.resolve_declaration(declaration) {
                        println!("       source: {url}");
                        break;
                    }
                }
            }
        }
        SearchDocument::Package(package) => {
            let mut details = Vec::new();

            if let Some(pname) = package.pname {
                details.push(pname);
            }

            if let Some(version) = package.version {
                details.push(version);
            }

            if !details.is_empty() {
                println!("       {}", details.join(" "));
            }

            if let Some(description) = package.description {
                let summary = description.lines().next().unwrap_or("").trim();

                if !summary.is_empty() {
                    println!("       {summary}");
                }
            }

            if let (Some(resolver), Some(position)) = (resolver, package.position.as_deref())
                && let Some(url) = resolver.resolve_package_position(position)
            {
                println!("       source: {url}");
            }
        }
    }
}

fn source_link_config_for_document<'a>(
    config: &'a AppConfig,
    common: &CommonDoc,
) -> Option<&'a SourceLinkConfig> {
    let source = config.sources.get(&common.source)?;

    let ref_config = source
        .refs
        .iter()
        .find(|ref_config| ref_config.id == common.ref_id)?;

    ref_config.source_links.as_ref()
}
