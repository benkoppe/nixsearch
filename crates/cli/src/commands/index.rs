use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};

use nixsearch_config::app::AppConfig;
use nixsearch_index::store::IndexStore;
use nixsearch_ops::cleanup;
use nixsearch_ops::generate::build_and_publish_generation;
use nixsearch_ops::lock::acquire_update_lock;
use nixsearch_ops::produce::artifact_store_from_config;
use nixsearch_ops::targets::{TargetKey, select_targets};

use crate::args::{ConfigArgs, SelectionArgs};

use super::load_required_config;

pub(super) async fn rebuild(args: SelectionArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;
    let update_lock = acquire_update_lock(&config.data.index_dir)?;

    let store = artifact_store_from_config(&config)?;
    let targets = select_targets(&config, args.source.as_deref(), args.ref_id.as_deref())?;

    if targets.is_empty() {
        bail!("no refs matched selection");
    }

    let index_store = IndexStore::new(&config.data.index_dir);
    let refresh_keys: BTreeSet<TargetKey> = targets.iter().map(TargetKey::from).collect();

    build_and_publish_generation(&index_store, &store, targets, &refresh_keys).await?;

    let report = cleanup::cleanup_under_lock(&config, &update_lock).await?;
    cleanup::log_report(&report);

    Ok(())
}

pub(super) fn inspect(args: ConfigArgs) -> Result<()> {
    let config = AppConfig::load(args.config.as_deref()).context("failed to load config")?;
    let index_store = IndexStore::new(&config.data.index_dir);

    let current_path = index_store.current_path()?;
    let manifest = index_store.current_manifest()?;

    println!("current index");
    println!("  path = {}", current_path.as_str());
    println!("  schema_version = {}", manifest.schema_version);
    println!("  generated_at = {}", manifest.generated_at);
    println!("  generation_id = {}", manifest.generation_id);
    println!("  documents = {}", manifest.document_count);
    println!("  targets = {}", manifest.targets.len());

    for target in manifest.targets {
        println!(
            "    {}/{} {:?} documents={}",
            target.source, target.ref_id, target.artifact_kind, target.document_count
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
