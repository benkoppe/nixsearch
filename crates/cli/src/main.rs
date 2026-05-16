use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use tracing::warn;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use tracing::info;

use nix_search_config::{
    AppConfig, DatasetConfig, DatasetKind, ProducerConfig, ProjectConfig, RefConfig,
};
use nix_search_core::{ArtifactKind, SearchDocument};
use nix_search_index::{SearchHit, SearchIndex};
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
    /// Build indexes from already-produces artifacts.
    Build(SelectionArgs),
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
            IndexCommand::Build(args) => index_build(args).await,
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
    let targets = select_targets(&config, &args)?;

    if targets.is_empty() {
        bail!("no refs matched selection");
    }

    for target in targets {
        info!(
            project = target.project_id,
            dataset = target.dataset_id,
            ref_id = target.ref_config.id,
            "updating ref"
        );

        let produced = produce_target(&store, &target).await?;
        build_index_for_produced_artifact(&config, &store, &target, &produced).await?;
    }

    Ok(())
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

async fn index_build(args: SelectionArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;
    let store = artifact_store_from_config(&config)?;
    let targets = select_targets(&config, &args)?;

    if targets.is_empty() {
        bail!("no refs matched selection");
    }

    for target in targets {
        build_index_for_existing_artifact(&config, &store, &target).await?;
    }

    Ok(())
}

fn search(args: SearchArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;

    let selection = SelectionArgs {
        config: args.config,
        project: args.project,
        dataset: args.dataset,
        ref_id: args.ref_id,
    };

    let targets = select_targets(&config, &selection)?;

    if targets.is_empty() {
        bail!("no refs matched selection");
    }

    let mut all_hits = Vec::new();

    for target in targets {
        let index_dir = index_dir_for_target(&config, &target);

        if !index_dir.exists() {
            warn!(
                index_dir = %index_dir.display(),
                project = target.project_id,
                dataset = target.dataset_id,
                ref_id = target.ref_config.id,
                "skipping missing index"
            );
            continue;
        }

        let index = SearchIndex::open(&index_dir)
            .with_context(|| format!("failed to open index {}", index_dir.display()))?;

        let hits = index
            .search(&args.query, args.limit)
            .with_context(|| format!("failed to search index {}", index_dir.display()))?;

        all_hits.extend(hits);
    }

    all_hits.sort_by(compare_hits_by_score_desc);
    all_hits.truncate(args.limit);

    for hit in all_hits {
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

async fn build_index_for_existing_artifact(
    config: &AppConfig,
    store: &ArtifactStore,
    target: &TargetRef,
) -> Result<()> {
    let artifact_ref = latest_artifact_ref_for_target(target);
    let metadata = store.get_metadata(&artifact_ref).await.with_context(|| {
        format!(
            "failed to read artifact metadata for {}/{}/{}; run `nix-search artifact produce` or
 `nix-search update` first",
            target.project_id, target.dataset_id, target.ref_config.id
        )
    })?;

    let produced = ProducedArtifact {
        artifact_ref,
        metadata,
    };

    build_index_for_produced_artifact(config, store, target, &produced).await
}

async fn build_index_for_produced_artifact(
    config: &AppConfig,
    store: &ArtifactStore,
    target: &TargetRef,
    produced: &ProducedArtifact,
) -> Result<()> {
    let documents = consume_target(store, target, produced).await?;
    let index_dir = index_dir_for_target(config, target);

    write_index(&index_dir, &documents)?;

    println!(
        "indexed {} documents: {}/{}/{}",
        documents.len(),
        target.project_id,
        target.dataset_id,
        target.ref_config.id
    );
    println!("  index_dir = {}", index_dir.display());

    Ok(())
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

fn write_index(index_dir: &Path, documents: &[SearchDocument]) -> Result<()> {
    info!(count = documents.len(), "writing index");

    let index = SearchIndex::create_or_replace(index_dir)?;
    let mut writer = index.writer()?;

    for document in documents {
        writer.add_document(document)?;
    }

    writer.commit()?;

    Ok(())
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

fn index_dir_for_target(config: &AppConfig, target: &TargetRef) -> PathBuf {
    config
        .data
        .index_dir
        .join(&target.project_id)
        .join(&target.dataset_id)
        .join(&target.ref_config.id)
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

fn compare_hits_by_score_desc(left: &SearchHit, right: &SearchHit) -> Ordering {
    right
        .score
        .partial_cmp(&left.score)
        .unwrap_or(Ordering::Equal)
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
