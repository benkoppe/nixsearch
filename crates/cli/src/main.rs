use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};

use nix_search_config::{
    AppConfig, DatasetConfig, DatasetKind, ProducerConfig, ProjectConfig, RefConfig,
};
use nix_search_core::{ArtifactKind, SearchDocument};
use nix_search_index::{
    IndexGenerationManifest, IndexStore, IndexTargetManifest, SearchHit, SearchIndex, SearchOptions,
};
use nix_search_source::{
    ChannelPackagesJsonProducer, Consumer, ExistingFileProducer, NixBuildOptionsJsonProducer,
    OptionsJsonConsumer, PackagesJsonConsumer, ProduceRequest, ProducedArtifact, Producer,
};
use nix_search_store::{ArtifactRef, ArtifactStore};

#[derive(Debug, Parser)]
#[command(name = "nix-search")]
#[command(about = "Search Nix packages and options")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate and display configuration.
    CheckConfig(ConfigArgs),

    /// Produce artifacts and build indexes for configured refs.
    Update(SelectionArgs),

    /// Search configured indexes.
    Search(SearchArgs),

    /// Debug artifact production and metadata.
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },

    /// Debug index building and inspection.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ArtifactCommand {
    /// Produce raw artifacts only, without indexing.
    Produce(SelectionArgs),

    /// Inspect artifact metadata.
    Inspect(SelectionArgs),
}

#[derive(Debug, Subcommand)]
enum IndexCommand {
    /// Rebuild the current index from exactly the selected refs.
    Rebuild(SelectionArgs),

    /// Inspect the current published index generation.
    Inspect(ConfigArgs),
}

#[derive(Debug, Args)]
struct ConfigArgs {
    /// Path to config file. If omitted, only defaults and env vars are loaded.
    #[arg(long)]
    config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct SelectionArgs {
    /// Path to config file.
    #[arg(long)]
    config: PathBuf,

    /// Restrict to one project.
    #[arg(long)]
    project: Option<String>,

    /// Restrict to one dataset.
    #[arg(long)]
    dataset: Option<String>,

    /// Restrict to one ref.
    #[arg(long = "ref")]
    ref_id: Option<String>,
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// Search query.
    query: String,

    /// Path to config file.
    #[arg(long)]
    config: PathBuf,

    /// Restrict to one project.
    #[arg(long)]
    project: Option<String>,

    /// Restrict to one dataset.
    #[arg(long)]
    dataset: Option<String>,

    /// Restrict to one ref.
    #[arg(long = "ref")]
    ref_id: Option<String>,

    /// Maximum number of results.
    #[arg(long, default_value_t = 20)]
    limit: usize,
}

#[derive(Debug, Clone)]
struct TargetRef {
    project_id: String,
    dataset_id: String,
    dataset_kind: DatasetKind,
    ref_config: RefConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TargetKey {
    project: String,
    dataset: String,
    ref_id: String,
}

impl TargetKey {
    fn new(
        project: impl Into<String>,
        dataset: impl Into<String>,
        ref_id: impl Into<String>,
    ) -> Self {
        Self {
            project: project.into(),
            dataset: dataset.into(),
            ref_id: ref_id.into(),
        }
    }
}

impl From<&TargetRef> for TargetKey {
    fn from(target: &TargetRef) -> Self {
        Self::new(
            target.project_id.clone(),
            target.dataset_id.clone(),
            target.ref_config.id.clone(),
        )
    }
}

impl From<&IndexTargetManifest> for TargetKey {
    fn from(target: &IndexTargetManifest) -> Self {
        Self::new(
            target.project.clone(),
            target.dataset.clone(),
            target.ref_id.clone(),
        )
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::CheckConfig(args) => check_config(args),
        Command::Update(args) => update(args).await,
        Command::Search(args) => search(args),
        Command::Artifact { command } => match command {
            ArtifactCommand::Produce(args) => artifact_produce(args).await,
            ArtifactCommand::Inspect(args) => artifact_inspect(args).await,
        },
        Command::Index { command } => match command {
            IndexCommand::Rebuild(args) => index_rebuild(args).await,
            IndexCommand::Inspect(args) => index_inspect(args),
        },
    }
}

fn check_config(args: ConfigArgs) -> Result<()> {
    let config = AppConfig::load(args.config.as_deref()).context("configuration check failed")?;

    println!("configuration is valid");
    println!("artifact_url = {}", config.data.artifact_url);
    println!("index_dir = {}", config.data.index_dir.display());
    println!("listen = {}", config.server.listen);
    println!("projects = {}", config.projects.len());

    for (project_id, project) in &config.projects {
        print_project(project_id, project);
    }

    Ok(())
}

async fn update(args: SelectionArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;
    let store = artifact_store_from_config(&config)?;
    let selected_targets = select_targets(&config, &args)?;

    if selected_targets.is_empty() {
        bail!("no refs matched selection");
    }

    let index_store = IndexStore::new(&config.data.index_dir);

    let mut included_targets = current_manifest_targets(&config, &index_store)?;
    let selected_keys: BTreeSet<TargetKey> = selected_targets.iter().map(TargetKey::from).collect();

    for target in selected_targets {
        included_targets.insert(TargetKey::from(&target), target);
    }

    if included_targets.is_empty() {
        bail!("no refs available to index");
    }

    build_and_publish_generation(
        &index_store,
        &store,
        included_targets.into_values().collect(),
        &selected_keys,
    )
    .await
}

async fn artifact_produce(args: SelectionArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;
    let store = artifact_store_from_config(&config)?;
    let targets = select_targets(&config, &args)?;

    if targets.is_empty() {
        bail!("no refs matched selection");
    }

    for target in targets {
        let produced = produce_target(&store, &target).await?;
        print_artifact_metadata(&produced);
    }

    Ok(())
}

async fn artifact_inspect(args: SelectionArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;
    let store = artifact_store_from_config(&config)?;
    let targets = select_targets(&config, &args)?;

    if targets.is_empty() {
        bail!("no refs matched selection");
    }

    for target in targets {
        let artifact_ref = latest_artifact_ref_for_target(&target);
        let metadata = store.get_metadata(&artifact_ref).await.with_context(|| {
            format!(
                "failed to read metadata for {}/{}/{}",
                target.project_id, target.dataset_id, target.ref_config.id
            )
        })?;

        println!("artifact");
        println!("  project = {}", metadata.project);
        println!("  dataset = {}", metadata.dataset);
        println!("  ref = {}", metadata.ref_id);
        println!("  kind = {:?}", metadata.kind);
        println!("  producer = {}", metadata.producer);
        println!(
            "  revision = {}",
            metadata.revision.as_deref().unwrap_or("-")
        );
        println!("  source = {}", metadata.source.as_deref().unwrap_or("-"));
        println!("  hash = {}", metadata.content_hash);
        println!("  size = {}", metadata.size_bytes);
        println!("  produced_at = {}", metadata.produced_at);

        for warning in &metadata.warnings {
            println!("  warning = {warning}");
        }
    }

    Ok(())
}

async fn index_rebuild(args: SelectionArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;
    let store = artifact_store_from_config(&config)?;
    let targets = select_targets(&config, &args)?;

    if targets.is_empty() {
        bail!("no refs matched selection");
    }

    let index_store = IndexStore::new(&config.data.index_dir);
    let refresh_keys: BTreeSet<TargetKey> = targets.iter().map(TargetKey::from).collect();

    build_and_publish_generation(&index_store, &store, targets, &refresh_keys).await
}

async fn build_and_publish_generation(
    index_store: &IndexStore,
    artifact_store: &ArtifactStore,
    targets: Vec<TargetRef>,
    refresh_keys: &BTreeSet<TargetKey>,
) -> Result<()> {
    let generation_path = index_store.create_generation_path()?;

    let index = SearchIndex::create_or_replace(&generation_path)?;
    let mut writer = index.writer()?;

    let mut total_documents = 0usize;
    let mut manifest_targets = Vec::new();

    for target in targets {
        let key = TargetKey::from(&target);

        let produced = if refresh_keys.contains(&key) {
            produce_target(artifact_store, &target).await?
        } else {
            produced_from_existing_artifact(artifact_store, &target).await?
        };

        let documents = consume_target(artifact_store, &target, &produced).await?;

        for document in &documents {
            writer.add_document(document)?;
        }

        total_documents += documents.len();

        manifest_targets.push(IndexTargetManifest {
            project: target.project_id.clone(),
            dataset: target.dataset_id.clone(),
            ref_id: target.ref_config.id.clone(),
            artifact_kind: produced.artifact_ref.kind,
            document_count: documents.len(),
            artifact_hash: Some(produced.metadata.content_hash.clone()),
            revision: produced.metadata.revision.clone(),
        });

        println!(
            "{} {} documents: {}/{}/{}",
            if refresh_keys.contains(&key) {
                "refreshed"
            } else {
                "retained"
            },
            documents.len(),
            target.project_id,
            target.dataset_id,
            target.ref_config.id
        );
    }

    writer.commit()?;

    let manifest = IndexGenerationManifest::new(total_documents, manifest_targets);
    index_store.write_manifest(&generation_path, &manifest)?;
    index_store.publish(&generation_path)?;

    println!("published index generation");
    println!("  generation = {}", generation_path.display());
    println!("  documents = {total_documents}");

    Ok(())
}

fn index_inspect(args: ConfigArgs) -> Result<()> {
    let config = AppConfig::load(args.config.as_deref()).context("failed to load config")?;
    let index_store = IndexStore::new(&config.data.index_dir);

    let current_path = index_store.current_path()?;
    let manifest = index_store.current_manifest()?;

    println!("current index");
    println!("  path = {}", current_path.display());
    println!("  schema_version = {}", manifest.schema_version);
    println!("  generated_at = {}", manifest.generated_at);
    println!("  documents = {}", manifest.document_count);
    println!("  targets = {}", manifest.targets.len());

    for target in manifest.targets {
        println!(
            "    {}/{}/{} {:?} documents={}",
            target.project,
            target.dataset,
            target.ref_id,
            target.artifact_kind,
            target.document_count
        );

        if let Some(revision) = target.revision {
            println!("      revision = {revision}");
        }

        if let Some(hash) = target.artifact_hash {
            println!("      artifact_hash = {hash}");
        }
    }

    Ok(())
}

fn search(args: SearchArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;

    let index_store = IndexStore::new(&config.data.index_dir);
    let current_path = index_store.current_path()?;

    let index = SearchIndex::open(&current_path)
        .with_context(|| format!("failed to open current index {}", current_path.display()))?;

    let hits = index.search(SearchOptions {
        query: args.query,
        limit: args.limit,
        project: args.project,
        dataset: args.dataset,
        ref_id: args.ref_id,
    })?;

    for hit in hits {
        print_search_hit(hit);
    }

    Ok(())
}

async fn produce_target(store: &ArtifactStore, target: &TargetRef) -> Result<ProducedArtifact> {
    let request = ProduceRequest {
        project: target.project_id.clone(),
        dataset: target.dataset_id.clone(),
        ref_id: target.ref_config.id.clone(),
    };

    match &target.ref_config.producer {
        ProducerConfig::ExistingFile { path, artifact } => {
            let producer = ExistingFileProducer::new(path, *artifact);

            producer.produce(store, &request).await.with_context(|| {
                format!(
                    "failed to produce artifact for {}/{}/{}",
                    target.project_id, target.dataset_id, target.ref_config.id
                )
            })
        }

        ProducerConfig::NixBuildOptionsJson {
            source_ref,
            attribute,
            import_path,
            output_path,
        } => {
            let producer =
                NixBuildOptionsJsonProducer::new(source_ref, attribute, import_path, output_path);

            producer.produce(store, &request).await.with_context(|| {
                format!(
                    "failed to produce Nix-built options artifact for {}/{}/{}",
                    target.project_id, target.dataset_id, target.ref_config.id
                )
            })
        }

        ProducerConfig::ChannelPackagesJson { channel, url } => {
            let producer = ChannelPackagesJsonProducer::new(channel, url.clone());

            producer.produce(store, &request).await.with_context(|| {
                format!(
                    "failed to produce channel packages artifact for {}/{}/{}",
                    target.project_id, target.dataset_id, target.ref_config.id
                )
            })
        }

        unsupported => bail!(
            "producer {:?} is configured but not implemented yet",
            unsupported.kind()
        ),
    }
}

async fn produced_from_existing_artifact(
    store: &ArtifactStore,
    target: &TargetRef,
) -> Result<ProducedArtifact> {
    let artifact_ref = latest_artifact_ref_for_target(target);
    let metadata = store.get_metadata(&artifact_ref).await.with_context(|| {
        format!(
            "failed to read artifact metadata for retained target {}/{}/{}",
            target.project_id, target.dataset_id, target.ref_config.id
        )
    })?;

    Ok(ProducedArtifact {
        artifact_ref,
        metadata,
    })
}

async fn consume_target(
    store: &ArtifactStore,
    target: &TargetRef,
    produced: &ProducedArtifact,
) -> Result<Vec<SearchDocument>> {
    match (target.dataset_kind, produced.artifact_ref.kind) {
        (DatasetKind::Options | DatasetKind::Mixed, ArtifactKind::OptionsJson) => {
            let consumer = OptionsJsonConsumer;

            consumer.consume(store, produced).await.with_context(|| {
                format!(
                    "failed to consume options artifact for {}/{}/{}",
                    target.project_id, target.dataset_id, target.ref_config.id
                )
            })
        }

        (DatasetKind::Packages | DatasetKind::Mixed, ArtifactKind::PackagesJson) => {
            let consumer = PackagesJsonConsumer;

            consumer.consume(store, produced).await.with_context(|| {
                format!(
                    "failed to consume packages artifact for {}/{}/{}",
                    target.project_id, target.dataset_id, target.ref_config.id
                )
            })
        }

        (kind, artifact) => bail!(
            "no consumer implemented for dataset kind {:?} and artifact kind {:?}",
            kind,
            artifact
        ),
    }
}

fn load_required_config(path: &Path) -> Result<AppConfig> {
    AppConfig::load(Some(path)).with_context(|| format!("failed to load {}", path.display()))
}

fn artifact_store_from_config(config: &AppConfig) -> Result<ArtifactStore> {
    let artifact_path = file_url_to_path(&config.data.artifact_url)?;

    ArtifactStore::local(&artifact_path).with_context(|| {
        format!(
            "failed to open artifact store from {}",
            config.data.artifact_url
        )
    })
}

fn file_url_to_path(url: &str) -> Result<PathBuf> {
    let path = url.strip_prefix("file://").with_context(|| {
        format!("only file:// artifact_url values are currently supported: {url}")
    })?;

    if path.trim().is_empty() {
        bail!("file:// artifact_url must include a path");
    }

    Ok(PathBuf::from(path))
}

fn latest_artifact_ref_for_target(target: &TargetRef) -> ArtifactRef {
    ArtifactRef::latest(
        target.project_id.clone(),
        target.dataset_id.clone(),
        target.ref_config.id.clone(),
        artifact_kind_for_producer(&target.ref_config.producer),
    )
}

fn artifact_kind_for_producer(producer: &ProducerConfig) -> ArtifactKind {
    match producer {
        ProducerConfig::ExistingFile { artifact, .. } => *artifact,
        ProducerConfig::ChannelPackagesJson { .. } => ArtifactKind::PackagesJson,
        ProducerConfig::NixBuildOptionsJson { .. } => ArtifactKind::OptionsJson,
        ProducerConfig::EvalModules { .. } => ArtifactKind::OptionsJson,
        ProducerConfig::Download { artifact, .. } => *artifact,
        ProducerConfig::CustomCommand { artifact, .. } => *artifact,
        ProducerConfig::FlakeOutput { .. } => ArtifactKind::FlakeInfoJson,
    }
}

fn select_targets(config: &AppConfig, selection: &SelectionArgs) -> Result<Vec<TargetRef>> {
    if selection.dataset.is_some() && selection.project.is_none() {
        bail!("--dataset requires --project");
    }

    if selection.ref_id.is_some() && selection.dataset.is_none() {
        bail!("--ref requires --dataset");
    }

    let mut targets = Vec::new();

    for (project_id, project) in &config.projects {
        if selection
            .project
            .as_ref()
            .is_some_and(|selected| selected != project_id)
        {
            continue;
        }

        collect_project_targets(project_id, project, selection, &mut targets);
    }

    if let Some(project_id) = &selection.project
        && !config.projects.contains_key(project_id)
    {
        bail!("unknown project {project_id:?}");
    }

    Ok(targets)
}

fn collect_project_targets(
    project_id: &str,
    project: &ProjectConfig,
    selection: &SelectionArgs,
    targets: &mut Vec<TargetRef>,
) {
    for dataset in &project.datasets {
        if selection
            .dataset
            .as_ref()
            .is_some_and(|selected| selected != &dataset.id)
        {
            continue;
        }

        collect_dataset_targets(project_id, dataset, selection, targets);
    }
}

fn collect_dataset_targets(
    project_id: &str,
    dataset: &DatasetConfig,
    selection: &SelectionArgs,
    targets: &mut Vec<TargetRef>,
) {
    for ref_config in &dataset.refs {
        if selection
            .ref_id
            .as_ref()
            .is_some_and(|selected| selected != &ref_config.id)
        {
            continue;
        }

        targets.push(TargetRef {
            project_id: project_id.to_owned(),
            dataset_id: dataset.id.clone(),
            dataset_kind: dataset.kind,
            ref_config: ref_config.clone(),
        });
    }
}

fn print_project(project_id: &str, project: &ProjectConfig) {
    let name = project.name.as_deref().unwrap_or(project_id);
    println!("  project {project_id}: {name}");

    for dataset in &project.datasets {
        let name = dataset.name.as_deref().unwrap_or(&dataset.id);
        println!("    dataset {}: {} ({:?})", dataset.id, name, dataset.kind);

        for ref_config in &dataset.refs {
            println!(
                "      ref {}: producer={:?}",
                ref_config.id,
                ref_config.producer.kind()
            );
        }
    }
}

fn print_artifact_metadata(produced: &ProducedArtifact) {
    println!("produced artifact");
    println!("  project = {}", produced.metadata.project);
    println!("  dataset = {}", produced.metadata.dataset);
    println!("  ref = {}", produced.metadata.ref_id);
    println!("  kind = {:?}", produced.metadata.kind);
    println!("  producer = {}", produced.metadata.producer);
    println!(
        "  revision = {}",
        produced.metadata.revision.as_deref().unwrap_or("-")
    );
    println!(
        "  source = {}",
        produced.metadata.source.as_deref().unwrap_or("-")
    );
    println!("  hash = {}", produced.metadata.content_hash);
    println!("  size = {}", produced.metadata.size_bytes);
}

fn print_search_hit(hit: SearchHit) {
    let common = hit.document.common();

    println!(
        "{score:.3}  {kind}  {project}/{dataset}/{ref_id}  {name}",
        score = hit.score,
        kind = common.kind.as_str(),
        project = common.project,
        dataset = common.dataset,
        ref_id = common.ref_id,
        name = common.name,
    );

    if !common.name_parts.groups.is_empty() {
        println!("       groups: {}", common.name_parts.groups.join(", "));
    }

    match hit.document {
        SearchDocument::Option(option) => {
            if let Some(description) = option.description {
                let summary = description.lines().next().unwrap_or("").trim();

                if !summary.is_empty() {
                    println!("       {summary}");
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
        }
    }
}

fn current_manifest_targets(
    config: &AppConfig,
    index_store: &IndexStore,
) -> Result<BTreeMap<TargetKey, TargetRef>> {
    let Some(manifest) = index_store.try_current_manifest()? else {
        return Ok(BTreeMap::new());
    };

    let mut targets = BTreeMap::new();

    for manifest_target in &manifest.targets {
        let target = resolve_manifest_target(config, manifest_target)?;
        targets.insert(TargetKey::from(manifest_target), target);
    }

    Ok(targets)
}

fn resolve_manifest_target(
    config: &AppConfig,
    manifest_target: &IndexTargetManifest,
) -> Result<TargetRef> {
    let project = config
        .projects
        .get(&manifest_target.project)
        .with_context(|| {
            format!(
                "current index manifest contains unknown project {:?}",
                manifest_target.project
            )
        })?;

    let dataset = project
        .datasets
        .iter()
        .find(|dataset| dataset.id == manifest_target.dataset)
        .with_context(|| {
            format!(
                "current index manifest contains unknown dataset {:?} in project {:?}",
                manifest_target.dataset, manifest_target.project
            )
        })?;

    let ref_config = dataset
        .refs
        .iter()
        .find(|ref_config| ref_config.id == manifest_target.ref_id)
        .with_context(|| {
            format!(
                "current index manifest contains unknown ref {:?} in project {:?}, dataset {:?}",
                manifest_target.ref_id, manifest_target.project, manifest_target.dataset
            )
        })?;

    Ok(TargetRef {
        project_id: manifest_target.project.clone(),
        dataset_id: manifest_target.dataset.clone(),
        dataset_kind: dataset.kind,
        ref_config: ref_config.clone(),
    })
}
