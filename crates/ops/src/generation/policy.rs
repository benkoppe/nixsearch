use std::collections::BTreeSet;

use anyhow::{Result, bail};

use nixsearch_config::producer::ProducerConfig;
use nixsearch_source::error::NixCommandFailure;
use nixsearch_store::StoreError;

use crate::targets::{TargetKey, TargetRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GenerationFailurePolicy {
    Strict,
    TolerateBootstrapNixFailures,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProduceFailureDisposition {
    Fatal,
    TolerableSkip,
}

pub(crate) fn validate_generation_policy(
    targets: &[TargetRef],
    refresh_keys: &BTreeSet<TargetKey>,
    policy: GenerationFailurePolicy,
) -> Result<()> {
    if policy == GenerationFailurePolicy::TolerateBootstrapNixFailures {
        let target_keys = targets.iter().map(TargetKey::from).collect::<BTreeSet<_>>();

        if &target_keys != refresh_keys {
            bail!("tolerant bootstrap generation must refresh every target");
        }
    }

    Ok(())
}

pub(crate) fn validate_generation_success_requirements(
    policy: GenerationFailurePolicy,
    required_success_targets: Option<&BTreeSet<TargetKey>>,
    successful_targets: &[TargetKey],
) -> Result<()> {
    if policy != GenerationFailurePolicy::TolerateBootstrapNixFailures {
        return Ok(());
    }

    if successful_targets.is_empty() {
        bail!("bootstrap generation produced no targets; all configured targets failed");
    }

    if let Some(required_success_targets) = required_success_targets
        && !required_success_targets.is_empty()
        && !successful_targets
            .iter()
            .any(|target| required_success_targets.contains(target))
    {
        bail!("bootstrap generation produced no default search targets");
    }

    Ok(())
}

pub(crate) fn classify_bootstrap_produce_error(
    target: &TargetRef,
    error: &anyhow::Error,
) -> ProduceFailureDisposition {
    let Some(source_ref) = tolerable_nix_source_ref(target) else {
        return ProduceFailureDisposition::Fatal;
    };

    if !is_remote_nix_ref(source_ref) {
        return ProduceFailureDisposition::Fatal;
    }

    if error.chain().any(|cause| cause.is::<StoreError>()) {
        return ProduceFailureDisposition::Fatal;
    }

    let Some(failure) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<NixCommandFailure>())
    else {
        return ProduceFailureDisposition::Fatal;
    };

    if failure.status.code().is_some() {
        ProduceFailureDisposition::TolerableSkip
    } else {
        ProduceFailureDisposition::Fatal
    }
}

fn tolerable_nix_source_ref(target: &TargetRef) -> Option<&str> {
    match &target.ref_config.producer {
        ProducerConfig::FlakeFile { source_ref, .. }
        | ProducerConfig::NixBuildOptionsJson { source_ref, .. } => Some(source_ref),
        _ => None,
    }
}

fn is_remote_nix_ref(source_ref: &str) -> bool {
    if source_ref.is_empty()
        || source_ref.starts_with('/')
        || source_ref.starts_with("./")
        || source_ref.starts_with("../")
        || source_ref.starts_with("path:")
        || source_ref.starts_with("file:")
        || source_ref.starts_with("git+file:")
    {
        return false;
    }

    source_ref.starts_with("github:")
        || source_ref.starts_with("gitlab:")
        || source_ref.starts_with("sourcehut:")
        || source_ref.starts_with("git+https://")
        || source_ref.starts_with("git+http://")
        || source_ref.starts_with("git+ssh://")
        || source_ref.starts_with("git+git://")
        || source_ref.starts_with("https://")
        || source_ref.starts_with("http://")
        || source_ref.starts_with("ssh://")
        || source_ref.starts_with("git://")
}
