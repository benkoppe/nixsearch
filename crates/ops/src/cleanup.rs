use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use tokio::process::Command;

use nixsearch_config::app::AppConfig;
use nixsearch_index::generation_validator::GenerationValidator;
use nixsearch_index::manifest::IndexGenerationManifest;
use nixsearch_index::store::{GenerationLease, IndexStore, PublishedGeneration};

use crate::lock::{self, UpdateLock};

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub deleted_generations: Vec<Utf8PathBuf>,
    pub deleted_incomplete_generations: Vec<Utf8PathBuf>,
    pub deleted_generation_locks: Vec<Utf8PathBuf>,
    pub preserved_active_generations: Vec<Utf8PathBuf>,
    pub warnings: Vec<String>,
    pub nix_gc: Option<NixCleanupOutcome>,
    pub nix_optimise: Option<NixCleanupOutcome>,
}

#[derive(Debug, Clone)]
pub struct NixCleanupOutcome {
    pub operation: &'static str,
    pub command: String,
    pub success: bool,
    pub skipped: bool,
    pub status_code: Option<i32>,
}

const NIXSEARCH_GCROOTS_DIR: &str = "/nix/var/nix/gcroots/nixsearch-runtime";
const RUNTIME_GC_ROOT_MARKER: &str = "nixsearch-runtime-root.json";

#[derive(Debug)]
struct CompleteGeneration {
    path: Utf8PathBuf,
    generated_at: time::OffsetDateTime,
}

#[derive(Debug)]
struct RuntimeStoreRoots {
    required_current_exe: Vec<PathBuf>,
    auxiliary: Vec<PathBuf>,
}

#[derive(Debug)]
struct RuntimeGcRoots {
    path: PathBuf,
}

impl RuntimeGcRoots {
    fn cleanup(self, report: &mut CleanupReport) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            report.warnings.push(format!(
                "failed to delete temporary runtime GC roots directory {}: {error}",
                self.path.display()
            ));
        }
    }
}

pub async fn cleanup_locked(config: &AppConfig) -> Result<CleanupReport> {
    let update_lock = lock::acquire_update_lock(&config.data.index_dir)?;
    cleanup_under_lock(config, &update_lock).await
}

pub async fn cleanup_under_lock(
    config: &AppConfig,
    update_lock: &UpdateLock,
) -> Result<CleanupReport> {
    ensure_update_lock_matches_config(config, update_lock)?;

    let mut report = CleanupReport::default();

    prune_index_generations(config, &mut report);

    let runtime_gc_roots = if config.maintenance.nix_store.gc {
        prepare_runtime_gc_roots(&mut report).await
    } else {
        None
    };
    let runtime_roots_prepared = !config.maintenance.nix_store.gc || runtime_gc_roots.is_some();

    if config.maintenance.nix_store.optimise {
        report.nix_optimise =
            Some(run_nix_cleanup("optimise", &["store", "optimise"], &["--optimise"]).await);
    }

    if config.maintenance.nix_store.gc {
        report.nix_gc = Some(if runtime_roots_prepared {
            run_nix_cleanup("gc", &["store", "gc"], &["--gc"]).await
        } else {
            skipped_nix_cleanup("gc", "nix store gc")
        });
    }

    if let Some(runtime_gc_roots) = runtime_gc_roots {
        runtime_gc_roots.cleanup(&mut report);
    }

    Ok(report)
}

fn ensure_update_lock_matches_config(config: &AppConfig, update_lock: &UpdateLock) -> Result<()> {
    let expected_lock_path = lock::update_lock_path(&config.data.index_dir);
    if update_lock.path() == expected_lock_path {
        return Ok(());
    }

    anyhow::bail!(
        "maintenance lock {} does not protect configured index dir {}; expected {}",
        update_lock.path(),
        config.data.index_dir,
        expected_lock_path
    )
}

pub fn log_report(report: &CleanupReport) {
    for path in &report.deleted_generations {
        tracing::info!(generation = %path, "deleted old index generation");
    }

    for path in &report.deleted_incomplete_generations {
        tracing::info!(generation = %path, "deleted stale incomplete index generation");
    }

    for path in &report.deleted_generation_locks {
        tracing::info!(lock = %path, "deleted orphaned index generation lock");
    }

    for path in &report.preserved_active_generations {
        tracing::info!(generation = %path, "preserved active index generation");
    }

    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }

    if let Some(outcome) = &report.nix_gc {
        log_nix_outcome(outcome);
    }

    if let Some(outcome) = &report.nix_optimise {
        log_nix_outcome(outcome);
    }
}

fn log_nix_outcome(outcome: &NixCleanupOutcome) {
    if outcome.skipped {
        tracing::warn!(
            operation = outcome.operation,
            command = outcome.command,
            "skipped Nix store cleanup"
        );
    } else if outcome.success {
        tracing::info!(
            operation = outcome.operation,
            command = outcome.command,
            "completed Nix store cleanup"
        );
    } else {
        tracing::warn!(
            operation = outcome.operation,
            command = outcome.command,
            status = ?outcome.status_code,
            "Nix store cleanup failed"
        );
    }
}

fn prune_index_generations(config: &AppConfig, report: &mut CleanupReport) {
    let index_store = IndexStore::new(&config.data.index_dir);
    let validator = GenerationValidator::new(index_store.clone());
    let delete_failed_after = match config
        .maintenance
        .index_generations
        .parse_delete_failed_after()
    {
        Ok(duration) => duration,
        Err(error) => {
            report.warnings.push(format!(
                "failed to parse index generation cleanup age; skipping generation pruning: {error}"
            ));
            return;
        }
    };

    let current = current_generation_canonical(&index_store, report);
    let mut current_is_valid = false;

    let mut complete = Vec::new();
    let mut incomplete = Vec::new();

    let entries = match fs::read_dir(index_store.generations_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            prune_orphaned_generation_locks(&index_store, report);
            sync_generation_locks_dir_best_effort(&index_store, report);
            return;
        }
        Err(error) => {
            report.warnings.push(format!(
                "failed to read index generations directory {}: {error}",
                index_store.generations_dir()
            ));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to read index generation entry: {error}"));
                continue;
            }
        };

        let path = match Utf8PathBuf::from_path_buf(entry.path()) {
            Ok(path) => path,
            Err(path) => {
                report.warnings.push(format!(
                    "skipping non-UTF-8 index generation path {}",
                    path.display()
                ));
                continue;
            }
        };

        let Some(name) = path.file_name() else {
            continue;
        };

        if !name.starts_with("generation-") {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to read file type for {path}: {error}"));
                continue;
            }
        };

        if !file_type.is_dir() {
            report
                .warnings
                .push(format!("skipping non-directory generation entry {path}"));
            continue;
        }

        let canonical = match path.canonicalize_utf8() {
            Ok(path) => path,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to canonicalize generation {path}: {error}"));
                continue;
            }
        };

        let is_current = current
            .as_ref()
            .is_some_and(|current| current == &canonical);

        if is_current
            && current_generation_is_valid_for_cleanup(config, &index_store, &validator, &canonical)
        {
            current_is_valid = true;
            continue;
        }

        if let Some(manifest) =
            structurally_complete_generation_manifest(&index_store, &validator, &canonical)
        {
            if is_current {
                report.warnings.push(format!(
                    "current index generation {canonical} is structurally verified but SEO-degraded; preserving rollback generations"
                ));
            } else {
                complete.push(CompleteGeneration {
                    path: canonical,
                    generated_at: manifest.generated_at,
                });
            }

            continue;
        }

        if !is_current {
            incomplete.push(canonical);
        }
    }

    if current_is_valid {
        prune_complete_generations(config, &index_store, complete, report);
    } else {
        report.warnings.push(
            "current index generation is missing, invalid, or incomplete; preserving complete generations"
                .to_owned(),
        );
    }

    prune_incomplete_generations(&index_store, incomplete, delete_failed_after, report);
    prune_orphaned_generation_locks(&index_store, report);

    sync_dir_best_effort(&index_store.generations_dir(), report);
    sync_generation_locks_dir_best_effort(&index_store, report);
}

fn current_generation_canonical(
    index_store: &IndexStore,
    report: &mut CleanupReport,
) -> Option<Utf8PathBuf> {
    match index_store.try_current_path() {
        Ok(Some(path)) => match path.canonicalize_utf8() {
            Ok(path) => Some(path),
            Err(error) => {
                report.warnings.push(format!(
                    "failed to canonicalize current index generation {path}: {error}"
                ));
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            report.warnings.push(format!(
                "failed to read current index generation: {error:#}"
            ));
            None
        }
    }
}

fn current_generation_is_valid_for_cleanup(
    config: &AppConfig,
    index_store: &IndexStore,
    validator: &GenerationValidator,
    path: &Utf8Path,
) -> bool {
    if config.public_seo_enabled() {
        seo_verified_generation_manifest(index_store, validator, path).is_some()
    } else {
        structurally_complete_generation_manifest(index_store, validator, path).is_some()
    }
}

fn structurally_complete_generation_manifest(
    index_store: &IndexStore,
    validator: &GenerationValidator,
    path: &Utf8Path,
) -> Option<IndexGenerationManifest> {
    let manifest = index_store.read_manifest(path).ok()?;
    validator
        .open_structurally_verified_published_generation(&PublishedGeneration {
            path: path.to_owned(),
            manifest: manifest.clone(),
        })
        .ok()?;
    Some(manifest)
}

fn seo_verified_generation_manifest(
    index_store: &IndexStore,
    validator: &GenerationValidator,
    path: &Utf8Path,
) -> Option<IndexGenerationManifest> {
    let manifest = index_store.read_manifest(path).ok()?;
    validator
        .validate_seo_verified_published_generation_unleased(&PublishedGeneration {
            path: path.to_owned(),
            manifest: manifest.clone(),
        })
        .ok()?;
    Some(manifest)
}

fn prune_complete_generations(
    config: &AppConfig,
    index_store: &IndexStore,
    mut complete: Vec<CompleteGeneration>,
    report: &mut CleanupReport,
) {
    complete.sort_by(|a, b| {
        b.generated_at
            .cmp(&a.generated_at)
            .then_with(|| a.path.as_str().cmp(b.path.as_str()))
    });

    let keep_non_current = config.maintenance.index_generations.keep.saturating_sub(1);

    for generation in complete.into_iter().skip(keep_non_current) {
        let Some(_lease) =
            try_acquire_cleanup_generation_lease(index_store, &generation.path, report)
        else {
            continue;
        };

        match fs::remove_dir_all(&generation.path) {
            Ok(()) => report.deleted_generations.push(generation.path),
            Err(error) => report.warnings.push(format!(
                "failed to delete old index generation {}: {error}",
                generation.path
            )),
        }
    }
}

fn prune_incomplete_generations(
    index_store: &IndexStore,
    incomplete: Vec<Utf8PathBuf>,
    delete_failed_after: Duration,
    report: &mut CleanupReport,
) {
    for path in incomplete {
        if !is_stale_incomplete_generation(&path, delete_failed_after, report) {
            continue;
        }

        let Some(_lease) = try_acquire_cleanup_generation_lease(index_store, &path, report) else {
            continue;
        };

        match fs::remove_dir_all(&path) {
            Ok(()) => report.deleted_incomplete_generations.push(path),
            Err(error) => report.warnings.push(format!(
                "failed to delete stale incomplete index generation {path}: {error}"
            )),
        }
    }
}

fn try_acquire_cleanup_generation_lease(
    index_store: &IndexStore,
    path: &Utf8Path,
    report: &mut CleanupReport,
) -> Option<GenerationLease> {
    match index_store.try_acquire_exclusive_generation_lease(path) {
        Ok(Some(lease)) => Some(lease),
        Ok(None) => {
            report.preserved_active_generations.push(path.to_owned());
            None
        }
        Err(error) => {
            report.warnings.push(format!(
                "failed to check active generation lease for {path}: {error:#}"
            ));
            None
        }
    }
}

fn prune_orphaned_generation_locks(index_store: &IndexStore, report: &mut CleanupReport) {
    let entries = match fs::read_dir(index_store.generation_locks_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(error) => {
            report.warnings.push(format!(
                "failed to read index generation locks directory {}: {error}",
                index_store.generation_locks_dir()
            ));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                report.warnings.push(format!(
                    "failed to read index generation lock entry: {error}"
                ));
                continue;
            }
        };

        let path = match Utf8PathBuf::from_path_buf(entry.path()) {
            Ok(path) => path,
            Err(path) => {
                report.warnings.push(format!(
                    "skipping non-UTF-8 index generation lock path {}",
                    path.display()
                ));
                continue;
            }
        };

        let Some(name) = path.file_name() else {
            continue;
        };
        let Some(generation_name) = name.strip_suffix(".lock") else {
            continue;
        };

        if !generation_name.starts_with("generation-") {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                report
                    .warnings
                    .push(format!("failed to read file type for {path}: {error}"));
                continue;
            }
        };

        if !file_type.is_file() {
            continue;
        }

        if index_store.generations_dir().join(generation_name).exists() {
            continue;
        }

        let _lease =
            match index_store.try_acquire_existing_exclusive_generation_lock(generation_name) {
                Ok(Some(lease)) => lease,
                Ok(None) => continue,
                Err(error) => {
                    report.warnings.push(format!(
                        "failed to check orphaned index generation lock {path}: {error:#}"
                    ));
                    continue;
                }
            };

        if index_store.generations_dir().join(generation_name).exists() {
            continue;
        }

        match fs::remove_file(&path) {
            Ok(()) => report.deleted_generation_locks.push(path),
            Err(error) => report.warnings.push(format!(
                "failed to delete orphaned index generation lock {path}: {error}"
            )),
        }
    }
}

fn is_stale_incomplete_generation(
    path: &Utf8Path,
    delete_failed_after: Duration,
    report: &mut CleanupReport,
) -> bool {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            report
                .warnings
                .push(format!("failed to read metadata for {path}: {error}"));
            return false;
        }
    };

    let modified = match metadata.modified() {
        Ok(modified) => modified,
        Err(error) => {
            report
                .warnings
                .push(format!("failed to read modified time for {path}: {error}"));
            return false;
        }
    };

    match modified.elapsed() {
        Ok(age) => age >= delete_failed_after,
        Err(error) => {
            report.warnings.push(format!(
                "modified time for {path} is in the future: {error}"
            ));
            false
        }
    }
}

fn sync_dir_best_effort(path: &Utf8Path, report: &mut CleanupReport) {
    if let Err(error) = fs::File::open(path).and_then(|file| file.sync_all()) {
        report.warnings.push(format!(
            "failed to sync generation directory {path}: {error}"
        ));
    }
}

fn sync_generation_locks_dir_best_effort(index_store: &IndexStore, report: &mut CleanupReport) {
    if index_store.generation_locks_dir().exists() {
        sync_dir_best_effort(&index_store.generation_locks_dir(), report);
    }
}

async fn prepare_runtime_gc_roots(report: &mut CleanupReport) -> Option<RuntimeGcRoots> {
    let roots = runtime_store_roots(report)?;

    if roots.required_current_exe.is_empty() && roots.auxiliary.is_empty() {
        report.warnings.push(
            "failed to identify any runtime Nix store paths; skipping Nix store GC".to_owned(),
        );
        return None;
    }

    let roots_dir = Path::new(NIXSEARCH_GCROOTS_DIR);
    if let Err(error) = fs::create_dir_all(roots_dir) {
        report.warnings.push(format!(
            "failed to create runtime GC roots directory {}: {error}; skipping Nix store GC",
            roots_dir.display()
        ));
        return None;
    }

    let run_roots = create_runtime_gc_roots_dir(roots_dir, report)?;

    if let Err(error) = write_runtime_gc_root_marker(&run_roots.path, &roots) {
        report.warnings.push(format!(
            "failed to write runtime GC roots marker in {}: {error}",
            run_roots.path.display()
        ));
    }

    for root in &roots.required_current_exe {
        let link = runtime_gc_root_link_path(&run_roots.path, root);
        if let Err(error) = add_runtime_gc_root(root, &link).await {
            report.warnings.push(format!(
                "failed to create required runtime GC root {} -> {}; skipping Nix store GC: {error}",
                link.display(),
                root.display()
            ));
            run_roots.cleanup(report);
            return None;
        }
    }

    let mut rooted_any = !roots.required_current_exe.is_empty();
    for root in &roots.auxiliary {
        let link = runtime_gc_root_link_path(&run_roots.path, root);
        if let Err(error) = add_runtime_gc_root(root, &link).await {
            report.warnings.push(format!(
                "failed to create auxiliary runtime GC root {} -> {}: {error}",
                link.display(),
                root.display()
            ));
            continue;
        }

        rooted_any = true;
    }

    if !rooted_any {
        report
            .warnings
            .push("failed to create any runtime GC roots; skipping Nix store GC".to_owned());
        run_roots.cleanup(report);
        return None;
    }

    sync_std_dir_best_effort(&run_roots.path, report);
    sync_std_dir_best_effort(roots_dir, report);
    Some(run_roots)
}

fn runtime_store_roots(report: &mut CleanupReport) -> Option<RuntimeStoreRoots> {
    let required_current_exe = required_current_exe_store_roots(report)?;
    let auxiliary = auxiliary_runtime_store_roots(&required_current_exe);

    Some(RuntimeStoreRoots {
        required_current_exe,
        auxiliary,
    })
}

fn required_current_exe_store_roots(report: &mut CleanupReport) -> Option<Vec<PathBuf>> {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            report.warnings.push(format!(
                "failed to resolve current executable; skipping Nix store GC: {error}"
            ));
            return None;
        }
    };

    let mut roots = current_exe_store_roots_from_paths(&current_exe, None);
    match current_exe.canonicalize() {
        Ok(canonical) => {
            push_store_roots_for_path_without_canonicalize(&canonical, &mut roots);
        }
        Err(error) if roots.is_empty() => {
            report.warnings.push(format!(
                "failed to canonicalize current executable {}; skipping Nix store GC: {error}",
                current_exe.display()
            ));
            return None;
        }
        Err(error) => {
            report.warnings.push(format!(
                "failed to canonicalize current executable {}; relying on lexical Nix store root: {error}",
                current_exe.display()
            ));
        }
    }

    Some(roots)
}

fn auxiliary_runtime_store_roots(required_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    for command in ["nixsearch", "nix", "nix-store"] {
        if let Some(path) = command_path(command) {
            candidates.push(path);
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&paths));
    }

    for variable in ["SSL_CERT_FILE", "NIX_SSL_CERT_FILE"] {
        if let Some(value) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(value));
        }
    }

    if let Some(value) = std::env::var_os("NIX_PATH") {
        candidates.extend(nix_path_store_candidates(&value));
    }

    store_roots_for_candidates_excluding(candidates, required_roots)
}

fn command_path(command: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;

    for dir in std::env::split_paths(&paths) {
        let path = dir.join(command);
        if path.is_file() {
            return Some(path);
        }
    }

    None
}

fn nix_path_store_candidates(value: &std::ffi::OsStr) -> Vec<PathBuf> {
    value
        .to_string_lossy()
        .split(':')
        .filter_map(|entry| {
            let path = entry.split_once('=').map_or(entry, |(_, path)| path);
            path.starts_with("/nix/store/").then(|| PathBuf::from(path))
        })
        .collect()
}

fn store_roots_for_candidates_excluding(
    candidates: Vec<PathBuf>,
    excluded_roots: &[PathBuf],
) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    for candidate in candidates {
        push_store_roots_for_path(&candidate, &mut roots);
    }

    roots.retain(|root| !excluded_roots.iter().any(|excluded| excluded == root));
    roots
}

fn push_store_roots_for_path(path: &Path, roots: &mut Vec<PathBuf>) {
    push_store_roots_for_path_without_canonicalize(path, roots);

    if let Ok(canonical) = path.canonicalize()
        && let Some(root) = store_root_from_path(&canonical)
    {
        push_unique_root(roots, root);
    }
}

fn push_store_roots_for_path_without_canonicalize(path: &Path, roots: &mut Vec<PathBuf>) {
    if let Some(root) = store_root_from_path(path) {
        push_unique_root(roots, root);
    }
}

fn push_unique_root(roots: &mut Vec<PathBuf>, root: PathBuf) {
    if !roots.iter().any(|existing| existing == &root) {
        roots.push(root);
    }
}

fn store_root_from_path(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();

    match (components.next(), components.next(), components.next()) {
        (
            Some(std::path::Component::RootDir),
            Some(std::path::Component::Normal(nix)),
            Some(std::path::Component::Normal(store)),
        ) if nix == "nix" && store == "store" => {}
        _ => return None,
    }

    let store_entry = components.next()?;
    let std::path::Component::Normal(store_entry) = store_entry else {
        return None;
    };

    Some(PathBuf::from("/nix/store").join(store_entry))
}

fn current_exe_store_roots_from_paths(
    current_exe: &Path,
    canonical: Option<&Path>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    push_store_roots_for_path_without_canonicalize(current_exe, &mut roots);
    if let Some(canonical) = canonical {
        push_store_roots_for_path_without_canonicalize(canonical, &mut roots);
    }

    roots
}

fn create_runtime_gc_roots_dir(
    roots_dir: &Path,
    report: &mut CleanupReport,
) -> Option<RuntimeGcRoots> {
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();

    for attempt in 0..100 {
        let path = roots_dir.join(format!("run-{}-{timestamp}-{attempt}", std::process::id()));

        match fs::create_dir(&path) {
            Ok(()) => return Some(RuntimeGcRoots { path }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                report.warnings.push(format!(
                    "failed to create temporary runtime GC roots directory {}: {error}; skipping Nix store GC",
                    path.display()
                ));
                return None;
            }
        }
    }

    report.warnings.push(format!(
        "failed to create unique temporary runtime GC roots directory in {}; skipping Nix store GC",
        roots_dir.display()
    ));
    None
}

fn write_runtime_gc_root_marker(path: &Path, roots: &RuntimeStoreRoots) -> Result<()> {
    let required_current_exe_roots = roots
        .required_current_exe
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>();
    let auxiliary_roots = roots
        .auxiliary
        .iter()
        .map(|root| root.display().to_string())
        .collect::<Vec<_>>();
    let marker = serde_json::json!({
        "schema_version": 1,
        "pid": std::process::id(),
        "created_at": time::OffsetDateTime::now_utc(),
        "required_current_exe_roots": required_current_exe_roots,
        "auxiliary_roots": auxiliary_roots,
    });

    fs::write(
        path.join(RUNTIME_GC_ROOT_MARKER),
        serde_json::to_vec_pretty(&marker)?,
    )?;

    Ok(())
}

fn runtime_gc_root_link_path(roots_dir: &Path, root: &Path) -> PathBuf {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown-store-root");

    roots_dir.join(name)
}

async fn add_runtime_gc_root(root: &Path, link: &Path) -> std::result::Result<(), String> {
    let output = Command::new("nix-store")
        .arg("--add-root")
        .arg(link)
        .arg("--realise")
        .arg(root)
        .output()
        .await
        .map_err(|error| error.to_string())?;

    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

fn sync_std_dir_best_effort(path: &Path, report: &mut CleanupReport) {
    if let Err(error) = fs::File::open(path).and_then(|file| file.sync_all()) {
        report.warnings.push(format!(
            "failed to sync runtime GC roots directory {}: {error}",
            path.display()
        ));
    }
}

async fn run_nix_cleanup(
    operation: &'static str,
    primary_args: &[&str],
    fallback_args: &[&str],
) -> NixCleanupOutcome {
    let primary_command = command_display("nix", primary_args);

    let primary = match Command::new("nix").args(primary_args).output().await {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return NixCleanupOutcome {
                operation,
                command: primary_command,
                success: false,
                skipped: true,
                status_code: None,
            };
        }
        Err(_) => {
            return NixCleanupOutcome {
                operation,
                command: primary_command,
                success: false,
                skipped: false,
                status_code: None,
            };
        }
    };

    if primary.status.success() {
        return NixCleanupOutcome {
            operation,
            command: primary_command,
            success: true,
            skipped: false,
            status_code: primary.status.code(),
        };
    }

    let stderr = String::from_utf8_lossy(&primary.stderr);
    if !should_try_legacy_nix_store(&stderr) {
        return NixCleanupOutcome {
            operation,
            command: primary_command,
            success: false,
            skipped: false,
            status_code: primary.status.code(),
        };
    }

    let fallback_command = command_display("nix-store", fallback_args);
    match Command::new("nix-store").args(fallback_args).output().await {
        Ok(output) => NixCleanupOutcome {
            operation,
            command: fallback_command,
            success: output.status.success(),
            skipped: false,
            status_code: output.status.code(),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => NixCleanupOutcome {
            operation,
            command: fallback_command,
            success: false,
            skipped: true,
            status_code: None,
        },
        Err(_) => NixCleanupOutcome {
            operation,
            command: fallback_command,
            success: false,
            skipped: false,
            status_code: None,
        },
    }
}

fn skipped_nix_cleanup(operation: &'static str, command: &str) -> NixCleanupOutcome {
    NixCleanupOutcome {
        operation,
        command: command.to_owned(),
        success: false,
        skipped: true,
        status_code: None,
    }
}

fn command_display(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn should_try_legacy_nix_store(stderr: &str) -> bool {
    let stderr = stderr.to_ascii_lowercase();

    stderr.contains("experimental")
        || stderr.contains("unknown command")
        || stderr.contains("unrecognised")
        || stderr.contains("unrecognized")
        || stderr.contains("invalid command")
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use camino::Utf8PathBuf;
    use nixsearch_index::seo_sidecar::SeoFactsArtifact;
    use nixsearch_index::store::IndexStore;
    use nixsearch_index_test_support::publish_canonical_index_with_generated_at;
    use tempfile::tempdir;
    use time::Duration as TimeDuration;

    use crate::cleanup::{
        CleanupReport, RUNTIME_GC_ROOT_MARKER, RuntimeGcRoots, cleanup_under_lock,
        current_exe_store_roots_from_paths, push_store_roots_for_path, runtime_gc_root_link_path,
        should_try_legacy_nix_store, store_root_from_path, write_runtime_gc_root_marker,
    };

    const STALE_IMMEDIATELY: &str = "0.000000001s";

    fn generation_lock_path(store: &IndexStore, generation: &Utf8PathBuf) -> Utf8PathBuf {
        let generation_name = generation
            .file_name()
            .expect("generation path should have a file name");
        store.generation_lock_path(generation_name)
    }

    #[tokio::test]
    async fn cleanup_preserves_generation_with_active_shared_lease() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let oldest =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let leased = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let retained = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2),
        );
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(3),
        );

        let store = IndexStore::new(&index_dir);
        let _lease = store.acquire_shared_generation_lease(&leased).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 2;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(retained.exists());
        assert!(leased.exists());
        assert!(!oldest.exists());
        assert_eq!(report.deleted_generations, vec![oldest]);
        assert_eq!(report.preserved_active_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_deletes_generation_after_shared_lease_drops() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let leased =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let retained = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2),
        );

        let store = IndexStore::new(&index_dir);
        let lease = store.acquire_shared_generation_lease(&leased).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 2;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(retained.exists());
        assert!(leased.exists());
        assert_eq!(report.preserved_active_generations, vec![leased.clone()]);

        drop(lease);

        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(retained.exists());
        assert!(!leased.exists());
        assert_eq!(report.deleted_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_preserves_complete_generations_when_public_current_sidecar_is_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let fallback =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        fs::remove_file(SeoFactsArtifact::path(&current)).unwrap();

        let mut config = nixsearch_test_support::app_config_with_public_url(&index_dir);
        config.maintenance.index_generations.keep = 1;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(fallback.exists());
        assert!(current.exists());
        assert!(report.deleted_generations.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("current index generation is missing"))
        );
    }

    #[tokio::test]
    async fn cleanup_prunes_normally_when_non_public_current_sidecar_is_missing() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let fallback =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let sidecar_path = SeoFactsArtifact::path(&current);
        fs::remove_file(&sidecar_path).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 1;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(!fallback.exists());
        assert!(current.exists());
        assert!(!sidecar_path.exists());
        assert_eq!(report.deleted_generations, vec![fallback]);
        assert!(report.warnings.is_empty());
    }

    #[tokio::test]
    async fn cleanup_preserves_stale_incomplete_generation_with_active_shared_lease() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let leased =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );

        let store = IndexStore::new(&index_dir);
        let _lease = store.acquire_shared_generation_lease(&leased).unwrap();
        fs::remove_file(store.manifest_path(&leased)).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.delete_failed_after = STALE_IMMEDIATELY.to_owned();

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(leased.exists());
        assert!(report.deleted_incomplete_generations.is_empty());
        assert_eq!(report.preserved_active_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_deletes_stale_incomplete_generation_after_shared_lease_drops() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let leased =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );

        let store = IndexStore::new(&index_dir);
        let lease = store.acquire_shared_generation_lease(&leased).unwrap();
        fs::remove_file(store.manifest_path(&leased)).unwrap();

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.delete_failed_after = STALE_IMMEDIATELY.to_owned();

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(leased.exists());
        assert!(report.deleted_incomplete_generations.is_empty());
        assert_eq!(report.preserved_active_generations, vec![leased.clone()]);

        drop(lease);

        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(current.exists());
        assert!(!leased.exists());
        assert_eq!(report.deleted_incomplete_generations, vec![leased]);
    }

    #[tokio::test]
    async fn cleanup_deletes_orphaned_generation_locks() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let oldest =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let retained = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(1),
        );
        let current = publish_canonical_index_with_generated_at(
            &index_dir,
            time::OffsetDateTime::UNIX_EPOCH + TimeDuration::hours(2),
        );

        let store = IndexStore::new(&index_dir);
        drop(store.acquire_shared_generation_lease(&oldest).unwrap());
        drop(store.acquire_shared_generation_lease(&retained).unwrap());
        drop(store.acquire_shared_generation_lease(&current).unwrap());

        let oldest_lock = generation_lock_path(&store, &oldest);
        let retained_lock = generation_lock_path(&store, &retained);
        let current_lock = generation_lock_path(&store, &current);
        assert!(oldest_lock.exists());
        assert!(retained_lock.exists());
        assert!(current_lock.exists());

        let mut config = nixsearch_test_support::app_config(&index_dir);
        config.maintenance.index_generations.keep = 2;

        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(!oldest.exists());
        assert!(retained.exists());
        assert!(current.exists());
        assert!(!oldest_lock.exists());
        assert!(retained_lock.exists());
        assert!(current_lock.exists());
        assert_eq!(report.deleted_generations, vec![oldest]);
        assert_eq!(report.deleted_generation_locks, vec![oldest_lock]);
    }

    #[tokio::test]
    async fn cleanup_preserves_active_orphaned_generation_locks() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();

        let generation =
            publish_canonical_index_with_generated_at(&index_dir, time::OffsetDateTime::UNIX_EPOCH);
        let store = IndexStore::new(&index_dir);
        let lease = store.acquire_shared_generation_lease(&generation).unwrap();
        let lock_path = generation_lock_path(&store, &generation);

        fs::remove_dir_all(&generation).unwrap();

        let config = nixsearch_test_support::app_config(&index_dir);
        let update_lock = crate::lock::acquire_update_lock(&index_dir).unwrap();
        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(lock_path.exists());
        assert!(report.deleted_generation_locks.is_empty());

        drop(lease);

        let report = cleanup_under_lock(&config, &update_lock).await.unwrap();

        assert!(!lock_path.exists());
        assert_eq!(report.deleted_generation_locks, vec![lock_path]);
    }

    #[tokio::test]
    async fn cleanup_under_lock_rejects_mismatched_update_lock() {
        let tempdir = tempdir().unwrap();
        let index_dir = Utf8PathBuf::from_path_buf(tempdir.path().join("indexes")).unwrap();
        let other_index_dir =
            Utf8PathBuf::from_path_buf(tempdir.path().join("other-indexes")).unwrap();
        let config = nixsearch_test_support::app_config(&index_dir);
        let update_lock = crate::lock::acquire_update_lock(&other_index_dir).unwrap();

        let error = cleanup_under_lock(&config, &update_lock).await.unwrap_err();

        assert!(format!("{error:#}").contains("does not protect configured index dir"));
    }

    #[test]
    fn legacy_fallback_detects_unsupported_new_nix_cli() {
        assert!(should_try_legacy_nix_store("unknown command 'store'"));
        assert!(should_try_legacy_nix_store(
            "experimental Nix feature 'nix-command' is disabled"
        ));
        assert!(!should_try_legacy_nix_store("network failed"));
    }

    #[test]
    fn store_root_from_path_extracts_top_level_store_path() {
        assert_eq!(
            store_root_from_path(Path::new("/nix/store/abc123-nix/bin/nix")).unwrap(),
            Path::new("/nix/store/abc123-nix")
        );
    }

    #[test]
    fn store_root_from_path_rejects_non_store_paths() {
        assert!(store_root_from_path(Path::new("/usr/bin/nix")).is_none());
    }

    #[test]
    fn push_store_roots_for_path_keeps_lexical_store_path() {
        let mut roots = Vec::new();

        push_store_roots_for_path(Path::new("/nix/store/env-path/bin/nixsearch"), &mut roots);

        assert_eq!(roots, vec![PathBuf::from("/nix/store/env-path")]);
    }

    #[test]
    fn push_store_roots_for_path_deduplicates_store_roots() {
        let mut roots = Vec::new();

        push_store_roots_for_path(Path::new("/nix/store/env-path/bin/nixsearch"), &mut roots);
        push_store_roots_for_path(Path::new("/nix/store/env-path/bin/nix"), &mut roots);

        assert_eq!(roots, vec![PathBuf::from("/nix/store/env-path")]);
    }

    #[test]
    fn current_exe_store_roots_include_lexical_and_canonical_store_paths() {
        let roots = current_exe_store_roots_from_paths(
            Path::new("/nix/store/lexical-nixsearch/bin/nixsearch"),
            Some(Path::new("/nix/store/canonical-nixsearch/bin/nixsearch")),
        );

        assert_eq!(
            roots,
            vec![
                PathBuf::from("/nix/store/lexical-nixsearch"),
                PathBuf::from("/nix/store/canonical-nixsearch"),
            ]
        );
    }

    #[test]
    fn current_exe_store_roots_allow_non_store_executable() {
        let roots = current_exe_store_roots_from_paths(Path::new("/usr/bin/nixsearch"), None);

        assert!(roots.is_empty());
    }

    #[test]
    fn current_exe_store_roots_use_canonical_store_path_for_wrappers() {
        let roots = current_exe_store_roots_from_paths(
            Path::new("/usr/local/bin/nixsearch"),
            Some(Path::new("/nix/store/canonical-nixsearch/bin/nixsearch")),
        );

        assert_eq!(roots, vec![PathBuf::from("/nix/store/canonical-nixsearch")]);
    }

    #[test]
    fn runtime_gc_root_link_path_uses_store_entry_name() {
        let link = runtime_gc_root_link_path(
            Path::new("/tmp/nixsearch-runtime/run-1"),
            Path::new("/nix/store/abc123-nixsearch"),
        );

        assert_eq!(
            link,
            PathBuf::from("/tmp/nixsearch-runtime/run-1/abc123-nixsearch")
        );
    }

    #[test]
    fn runtime_gc_root_marker_records_roots() {
        let tempdir = tempdir().unwrap();
        let roots = crate::cleanup::RuntimeStoreRoots {
            required_current_exe: vec![PathBuf::from("/nix/store/current-nixsearch")],
            auxiliary: vec![PathBuf::from("/nix/store/current-nix")],
        };

        write_runtime_gc_root_marker(tempdir.path(), &roots).unwrap();

        let marker = fs::read_to_string(tempdir.path().join(RUNTIME_GC_ROOT_MARKER)).unwrap();
        let marker: serde_json::Value = serde_json::from_str(&marker).unwrap();
        assert_eq!(marker["schema_version"], 1);
        assert_eq!(
            marker["required_current_exe_roots"],
            serde_json::json!(["/nix/store/current-nixsearch"])
        );
        assert_eq!(
            marker["auxiliary_roots"],
            serde_json::json!(["/nix/store/current-nix"])
        );
    }

    #[test]
    fn runtime_gc_roots_cleanup_removes_run_directory() {
        let tempdir = tempdir().unwrap();
        let run_dir = tempdir.path().join("run");
        fs::create_dir(&run_dir).unwrap();
        fs::write(run_dir.join("root"), b"root").unwrap();
        let mut report = CleanupReport::default();

        RuntimeGcRoots {
            path: run_dir.clone(),
        }
        .cleanup(&mut report);

        assert!(!run_dir.exists());
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn runtime_gc_roots_cleanup_warns_when_run_directory_removal_fails() {
        let tempdir = tempdir().unwrap();
        let missing = tempdir.path().join("missing-run");
        let mut report = CleanupReport::default();

        RuntimeGcRoots { path: missing }.cleanup(&mut report);

        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("failed to delete temporary runtime GC roots"));
    }
}
