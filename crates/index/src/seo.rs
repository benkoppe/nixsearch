use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use nixsearch_core::artifact::ArtifactKind;
use nixsearch_core::document::{DocumentKind, SearchDocument};

use crate::manifest::IndexGenerationManifest;

pub const SEO_SIDECAR_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeoSidecar {
    pub schema_version: u32,
    pub generation_id: String,
    pub refs: Vec<SeoRefFacts>,
    pub entries: Vec<SeoEntryFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeoRefFacts {
    pub source: String,
    pub ref_id: String,
    pub total_supported_indexed_count: usize,
    pub package_supported_count: usize,
    pub option_supported_count: usize,
    pub package_eligible_count: usize,
    pub option_eligible_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeoEntryFacts {
    pub source: String,
    pub ref_id: String,
    pub name: String,
    pub total_supported_indexed_count: usize,
    pub package_supported_count: usize,
    pub option_supported_count: usize,
    pub package_eligible_count: usize,
    pub option_eligible_count: usize,
}

#[derive(Debug, Default)]
pub struct SeoSidecarAccumulator {
    entries: BTreeMap<SeoEntryKey, SeoCounts>,
}

impl SeoSidecarAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, document: &SearchDocument) {
        if !document.kind().is_supported_indexed_entry() {
            return;
        }

        let common = document.common();
        let key = SeoEntryKey {
            source: common.source.clone(),
            ref_id: common.ref_id.clone(),
            name: common.name.clone(),
        };

        self.entries
            .entry(key)
            .or_default()
            .observe(document.kind(), document.is_seo_eligible_entry());
    }

    pub fn into_sidecar(self, generation_id: impl Into<String>) -> SeoSidecar {
        let mut refs = BTreeMap::<SeoRefKey, SeoCounts>::new();
        let mut entries = Vec::with_capacity(self.entries.len());

        for (key, counts) in self.entries {
            refs.entry(SeoRefKey {
                source: key.source.clone(),
                ref_id: key.ref_id.clone(),
            })
            .or_default()
            .merge(&counts);

            entries.push(SeoEntryFacts {
                source: key.source,
                ref_id: key.ref_id,
                name: key.name,
                total_supported_indexed_count: counts.total_supported_indexed_count,
                package_supported_count: counts.package_supported_count,
                option_supported_count: counts.option_supported_count,
                package_eligible_count: counts.package_eligible_count,
                option_eligible_count: counts.option_eligible_count,
            });
        }

        let refs = refs
            .into_iter()
            .map(|(key, counts)| SeoRefFacts {
                source: key.source,
                ref_id: key.ref_id,
                total_supported_indexed_count: counts.total_supported_indexed_count,
                package_supported_count: counts.package_supported_count,
                option_supported_count: counts.option_supported_count,
                package_eligible_count: counts.package_eligible_count,
                option_eligible_count: counts.option_eligible_count,
            })
            .collect();

        SeoSidecar {
            schema_version: SEO_SIDECAR_SCHEMA_VERSION,
            generation_id: generation_id.into(),
            refs,
            entries,
        }
    }
}

impl SeoSidecar {
    pub fn validate_for_manifest(&self, manifest: &IndexGenerationManifest) -> Result<()> {
        if self.schema_version != SEO_SIDECAR_SCHEMA_VERSION {
            bail!(
                "unsupported SEO sidecar schema version {}",
                self.schema_version
            );
        }

        if self.generation_id != manifest.generation_id {
            bail!(
                "SEO sidecar generation_id mismatch: stored {:?}, manifest {:?}",
                self.generation_id,
                manifest.generation_id
            );
        }

        let manifest_targets = manifest_target_kinds(manifest);
        let mut ref_sums = BTreeMap::<SeoRefKey, SeoCounts>::new();
        let mut seen_entries = BTreeSet::<SeoEntryKey>::new();

        for entry in &self.entries {
            entry.validate_counts().with_context(|| {
                format!(
                    "invalid SEO sidecar entry {}/{}/{}",
                    entry.source, entry.ref_id, entry.name
                )
            })?;

            let key = SeoEntryKey {
                source: entry.source.clone(),
                ref_id: entry.ref_id.clone(),
                name: entry.name.clone(),
            };

            if !seen_entries.insert(key) {
                bail!(
                    "duplicate SEO sidecar entry {}/{}/{}",
                    entry.source,
                    entry.ref_id,
                    entry.name
                );
            }

            validate_entry_manifest_targets(entry, &manifest_targets)?;

            ref_sums
                .entry(SeoRefKey {
                    source: entry.source.clone(),
                    ref_id: entry.ref_id.clone(),
                })
                .or_default()
                .merge(&SeoCounts::from_entry(entry));
        }

        let mut seen_refs = BTreeSet::<SeoRefKey>::new();

        for ref_facts in &self.refs {
            ref_facts.validate_counts().with_context(|| {
                format!(
                    "invalid SEO sidecar ref {}/{}",
                    ref_facts.source, ref_facts.ref_id
                )
            })?;

            let key = SeoRefKey {
                source: ref_facts.source.clone(),
                ref_id: ref_facts.ref_id.clone(),
            };

            if !seen_refs.insert(key.clone()) {
                bail!(
                    "duplicate SEO sidecar ref {}/{}",
                    ref_facts.source,
                    ref_facts.ref_id
                );
            }

            let Some(sum) = ref_sums.remove(&key) else {
                bail!(
                    "SEO sidecar ref {}/{} has no entries",
                    ref_facts.source,
                    ref_facts.ref_id
                );
            };

            let actual = SeoCounts::from_ref(ref_facts);
            if actual != sum {
                bail!(
                    "SEO sidecar ref totals mismatch for {}/{}",
                    ref_facts.source,
                    ref_facts.ref_id
                );
            }

            validate_ref_manifest_targets(ref_facts, &manifest_targets)?;
        }

        if let Some((key, _)) = ref_sums.into_iter().next() {
            bail!(
                "SEO sidecar entries for {}/{} are missing ref totals",
                key.source,
                key.ref_id
            );
        }

        Ok(())
    }
}

impl SeoEntryFacts {
    fn validate_counts(&self) -> Result<()> {
        validate_counts(
            self.total_supported_indexed_count,
            self.package_supported_count,
            self.option_supported_count,
            self.package_eligible_count,
            self.option_eligible_count,
        )
    }
}

impl SeoRefFacts {
    fn validate_counts(&self) -> Result<()> {
        validate_counts(
            self.total_supported_indexed_count,
            self.package_supported_count,
            self.option_supported_count,
            self.package_eligible_count,
            self.option_eligible_count,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SeoEntryKey {
    source: String,
    ref_id: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SeoRefKey {
    source: String,
    ref_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SeoCounts {
    total_supported_indexed_count: usize,
    package_supported_count: usize,
    option_supported_count: usize,
    package_eligible_count: usize,
    option_eligible_count: usize,
}

impl SeoCounts {
    fn observe(&mut self, kind: &DocumentKind, eligible: bool) {
        match kind {
            DocumentKind::Package => {
                self.total_supported_indexed_count += 1;
                self.package_supported_count += 1;

                if eligible {
                    self.package_eligible_count += 1;
                }
            }
            DocumentKind::Option => {
                self.total_supported_indexed_count += 1;
                self.option_supported_count += 1;

                if eligible {
                    self.option_eligible_count += 1;
                }
            }
            DocumentKind::App | DocumentKind::Service => {}
        }
    }

    fn merge(&mut self, other: &Self) {
        self.total_supported_indexed_count += other.total_supported_indexed_count;
        self.package_supported_count += other.package_supported_count;
        self.option_supported_count += other.option_supported_count;
        self.package_eligible_count += other.package_eligible_count;
        self.option_eligible_count += other.option_eligible_count;
    }

    fn from_entry(entry: &SeoEntryFacts) -> Self {
        Self {
            total_supported_indexed_count: entry.total_supported_indexed_count,
            package_supported_count: entry.package_supported_count,
            option_supported_count: entry.option_supported_count,
            package_eligible_count: entry.package_eligible_count,
            option_eligible_count: entry.option_eligible_count,
        }
    }

    fn from_ref(ref_facts: &SeoRefFacts) -> Self {
        Self {
            total_supported_indexed_count: ref_facts.total_supported_indexed_count,
            package_supported_count: ref_facts.package_supported_count,
            option_supported_count: ref_facts.option_supported_count,
            package_eligible_count: ref_facts.package_eligible_count,
            option_eligible_count: ref_facts.option_eligible_count,
        }
    }
}

fn validate_counts(
    total_supported_indexed_count: usize,
    package_supported_count: usize,
    option_supported_count: usize,
    package_eligible_count: usize,
    option_eligible_count: usize,
) -> Result<()> {
    if total_supported_indexed_count != package_supported_count + option_supported_count {
        bail!("total_supported_indexed_count does not match package+option counts");
    }

    if package_eligible_count > package_supported_count {
        bail!("package_eligible_count exceeds package_supported_count");
    }

    if option_eligible_count > option_supported_count {
        bail!("option_eligible_count exceeds option_supported_count");
    }

    Ok(())
}

fn manifest_target_kinds(
    manifest: &IndexGenerationManifest,
) -> BTreeMap<(String, String), BTreeSet<ArtifactKind>> {
    let mut targets = BTreeMap::<(String, String), BTreeSet<ArtifactKind>>::new();

    for target in &manifest.targets {
        targets
            .entry((target.source.clone(), target.ref_id.clone()))
            .or_default()
            .insert(target.artifact_kind);
    }

    targets
}

fn validate_entry_manifest_targets(
    entry: &SeoEntryFacts,
    manifest_targets: &BTreeMap<(String, String), BTreeSet<ArtifactKind>>,
) -> Result<()> {
    let Some(kinds) = manifest_targets.get(&(entry.source.clone(), entry.ref_id.clone())) else {
        bail!(
            "SEO sidecar entry {}/{}/{} references ref missing from manifest",
            entry.source,
            entry.ref_id,
            entry.name
        );
    };

    if entry.package_supported_count > 0 && !kinds.contains(&ArtifactKind::PackagesJson) {
        bail!(
            "SEO sidecar entry {}/{}/{} has package facts without package target",
            entry.source,
            entry.ref_id,
            entry.name
        );
    }

    if entry.option_supported_count > 0 && !kinds.contains(&ArtifactKind::OptionsJson) {
        bail!(
            "SEO sidecar entry {}/{}/{} has option facts without option target",
            entry.source,
            entry.ref_id,
            entry.name
        );
    }

    Ok(())
}

fn validate_ref_manifest_targets(
    ref_facts: &SeoRefFacts,
    manifest_targets: &BTreeMap<(String, String), BTreeSet<ArtifactKind>>,
) -> Result<()> {
    let Some(kinds) = manifest_targets.get(&(ref_facts.source.clone(), ref_facts.ref_id.clone()))
    else {
        bail!(
            "SEO sidecar ref {}/{} references ref missing from manifest",
            ref_facts.source,
            ref_facts.ref_id
        );
    };

    if ref_facts.package_supported_count > 0 && !kinds.contains(&ArtifactKind::PackagesJson) {
        bail!(
            "SEO sidecar ref {}/{} has package facts without package target",
            ref_facts.source,
            ref_facts.ref_id
        );
    }

    if ref_facts.option_supported_count > 0 && !kinds.contains(&ArtifactKind::OptionsJson) {
        bail!(
            "SEO sidecar ref {}/{} has option facts without option target",
            ref_facts.source,
            ref_facts.ref_id
        );
    }

    Ok(())
}
