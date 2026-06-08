use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use camino::{Utf8Path, Utf8PathBuf};
use tokio::process::Command;

use nixsearch_config::app::AppConfig;
use nixsearch_index::manifest::IndexGenerationManifest;
use nixsearch_index::search::SearchIndex;
use nixsearch_index::store::IndexStore;

use crate::lock::{self, UpdateLock};

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub deleted_generations: Vec<Utf8PathBuf>,
    pub deleted_incomplete_generations: Vec<Utf8PathBuf>,
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

#[derive(Debug)]
struct CompleteGeneration {
    path: Utf8PathBuf,
    generated_at: time::OffsetDateTime,
}

pub async fn cleanup_locked(config: &AppConfig) -> Result<CleanupReport> {
    let update_lock = lock::acquire_update_lock(&config.data.index_dir)?;
    Ok(cleanup_under_lock(config, &update_lock).await)
}

pub async fn cleanup_under_lock(config: &AppConfig, _update_lock: &UpdateLock) -> CleanupReport {
    let mut report = CleanupReport::default();

    prune_index_generations(config, &mut report);

    let runtime_roots_prepared =
        !config.maintenance.nix_store.gc || protect_runtime_gc_roots(&mut report).await;

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

    report
}

pub fn log_report(report: &CleanupReport) {
    for path in &report.deleted_generations {
        tracing::info!(generation = %path, "deleted old index generation");
    }

    for path in &report.deleted_incomplete_generations {
        tracing::info!(generation = %path, "deleted stale incomplete index generation");
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
    let current_is_complete = current
        .as_ref()
        .and_then(|path| complete_manifest(&index_store, path))
        .is_some();

    let mut complete = Vec::new();
    let mut incomplete = Vec::new();

    let entries = match fs::read_dir(index_store.generations_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
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

        if let Some(manifest) = complete_manifest(&index_store, &canonical) {
            if !is_current {
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

    if current_is_complete {
        prune_complete_generations(config, complete, report);
    } else {
        report.warnings.push(
            "current index generation is missing, invalid, or incomplete; preserving complete generations"
                .to_owned(),
        );
    }

    prune_incomplete_generations(incomplete, delete_failed_after, report);

    sync_dir_best_effort(&index_store.generations_dir(), report);
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

fn complete_manifest(index_store: &IndexStore, path: &Utf8Path) -> Option<IndexGenerationManifest> {
    let manifest = index_store.read_manifest(path).ok()?;
    SearchIndex::open(path).ok()?;
    Some(manifest)
}

fn prune_complete_generations(
    config: &AppConfig,
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
    incomplete: Vec<Utf8PathBuf>,
    delete_failed_after: Duration,
    report: &mut CleanupReport,
) {
    for path in incomplete {
        if !is_stale_incomplete_generation(&path, delete_failed_after, report) {
            continue;
        }

        match fs::remove_dir_all(&path) {
            Ok(()) => report.deleted_incomplete_generations.push(path),
            Err(error) => report.warnings.push(format!(
                "failed to delete stale incomplete index generation {path}: {error}"
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

async fn protect_runtime_gc_roots(report: &mut CleanupReport) -> bool {
    let roots = runtime_store_roots(report);

    if roots.is_empty() {
        report.warnings.push(
            "failed to identify any runtime Nix store paths; skipping Nix store GC".to_owned(),
        );
        return false;
    }

    let roots_dir = Path::new(NIXSEARCH_GCROOTS_DIR);
    if let Err(error) = fs::create_dir_all(roots_dir) {
        report.warnings.push(format!(
            "failed to create runtime GC roots directory {}: {error}; skipping Nix store GC",
            roots_dir.display()
        ));
        return false;
    }

    let mut rooted_any = false;
    for (index, root) in roots.iter().enumerate() {
        let Some(name) = root.file_name().and_then(|name| name.to_str()) else {
            report.warnings.push(format!(
                "failed to derive runtime GC root name for {}; skipping it",
                root.display()
            ));
            continue;
        };

        let link = roots_dir.join(format!("{index}-{name}"));
        if let Err(error) = add_runtime_gc_root(root, &link).await {
            report.warnings.push(format!(
                "failed to create runtime GC root {} -> {}: {error}",
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
        return false;
    }

    sync_std_dir_best_effort(roots_dir, report);
    true
}

fn runtime_store_roots(report: &mut CleanupReport) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    match std::env::current_exe() {
        Ok(path) => candidates.push(path),
        Err(error) => report
            .warnings
            .push(format!("failed to resolve current executable: {error}")),
    }

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

    store_roots_for_candidates(candidates)
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

fn store_roots_for_candidates(candidates: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();

    for candidate in candidates {
        push_store_roots_for_path(&candidate, &mut roots);
    }

    roots
}

fn push_store_roots_for_path(path: &Path, roots: &mut Vec<PathBuf>) {
    if let Some(root) = store_root_from_path(path) {
        push_unique_root(roots, root);
    }

    if let Ok(canonical) = path.canonicalize()
        && let Some(root) = store_root_from_path(&canonical)
    {
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

async fn add_runtime_gc_root(root: &Path, link: &Path) -> std::result::Result<(), String> {
    match fs::remove_file(link) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }

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
    use std::path::{Path, PathBuf};

    use crate::cleanup::{
        push_store_roots_for_path, should_try_legacy_nix_store, store_root_from_path,
    };

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
}
