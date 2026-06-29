use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};

use nixsearch_index::generation_validator::GenerationValidator;
use nixsearch_index::store::IndexStore;
use nixsearch_ops::cleanup;
use nixsearch_ops::generate::{RetainedGeneration, build_and_publish_generation};
use nixsearch_ops::lock::acquire_update_lock;
use nixsearch_ops::produce::artifact_store_from_config;
use nixsearch_ops::targets::{TargetKey, current_manifest_targets, select_targets};

use crate::args::SelectionArgs;

use super::load_required_config;

pub(super) async fn update(args: SelectionArgs) -> Result<()> {
    let config = load_required_config(&args.config)?;
    let update_lock = acquire_update_lock(&config.data.index_dir)?;

    let store = artifact_store_from_config(&config)?;
    let selected_targets = select_targets(&config, args.source.as_deref(), args.ref_id.as_deref())?;

    if selected_targets.is_empty() {
        bail!("no refs matched selection");
    }

    let index_store = IndexStore::new(&config.data.index_dir);
    let full_update = args.source.is_none() && args.ref_id.is_none();
    let selected_keys: BTreeSet<TargetKey> = selected_targets.iter().map(TargetKey::from).collect();

    let (mut included_targets, retained_generation) = if full_update {
        (BTreeMap::new(), None)
    } else {
        let leased_generation = index_store.current_leased_generation().with_context(|| {
            "partial update requires a readable current index generation; \
             run unfiltered `nixsearch update` to refresh all configured refs"
        })?;
        let complete = GenerationValidator::new(index_store.clone())
            .open_structurally_verified_published_generation(
                leased_generation.published_generation(),
            )
            .with_context(|| {
                "partial update requires a structurally verified current index generation; \
                 run unfiltered `nixsearch update` to refresh all configured refs"
            })?;
        let retained_generation =
            RetainedGeneration::from_index(leased_generation.manifest(), &complete.index)
                .context("failed to load retained current index documents")?;
        let included_targets =
            current_manifest_targets(&config, &index_store).with_context(|| {
                "partial update requires a readable current index manifest; \
             run unfiltered `nixsearch update` to refresh all configured refs"
            })?;

        (included_targets, Some(retained_generation))
    };

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
        retained_generation.as_ref(),
    )
    .await?;

    let report = cleanup::cleanup_under_lock(&config, &update_lock).await?;
    cleanup::log_report(&report);

    Ok(())
}
