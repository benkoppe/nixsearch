use anyhow::{Context, Result};

use nixsearch_config::app::AppConfig;
use nixsearch_ops::cleanup::{self, CleanupReport, NixCleanupOutcome};

use crate::args::ConfigArgs;

pub(super) async fn cleanup(args: ConfigArgs) -> Result<()> {
    let config = AppConfig::load(args.config.as_deref()).context("failed to load config")?;
    let report = cleanup::cleanup_locked(&config).await?;

    print_report(&report);

    Ok(())
}

fn print_report(report: &CleanupReport) {
    println!("maintenance cleanup");
    println!(
        "  deleted_generations = {}",
        report.deleted_generations.len()
    );
    println!(
        "  deleted_incomplete_generations = {}",
        report.deleted_incomplete_generations.len()
    );
    println!(
        "  deleted_generation_locks = {}",
        report.deleted_generation_locks.len()
    );
    println!(
        "  preserved_active_generations = {}",
        report.preserved_active_generations.len()
    );

    for path in &report.deleted_generations {
        println!("    deleted = {path}");
    }

    for path in &report.deleted_incomplete_generations {
        println!("    deleted_incomplete = {path}");
    }

    for path in &report.deleted_generation_locks {
        println!("    deleted_generation_lock = {path}");
    }

    for path in &report.preserved_active_generations {
        println!("    preserved_active = {path}");
    }

    if let Some(outcome) = &report.nix_gc {
        print_nix_outcome(outcome);
    }

    if let Some(outcome) = &report.nix_optimise {
        print_nix_outcome(outcome);
    }

    for warning in &report.warnings {
        println!("  warning = {warning}");
    }
}

fn print_nix_outcome(outcome: &NixCleanupOutcome) {
    println!(
        "  nix_{} = {}",
        outcome.operation,
        if outcome.success {
            "ok"
        } else if outcome.skipped {
            "skipped"
        } else {
            "failed"
        }
    );
    println!("    command = {}", outcome.command);

    if let Some(status_code) = outcome.status_code {
        println!("    status = {status_code}");
    }
}
