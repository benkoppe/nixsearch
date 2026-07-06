use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, bail};
use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use camino::{Utf8Path, Utf8PathBuf};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio_util::io::ReaderStream;

use nixsearch_config::app::AppConfig;
use nixsearch_index::store::PublishedGeneration;
use nixsearch_service::{SearchService, ServedGenerationSnapshot};

use crate::origin::configured_public_origin;
use crate::sitemap::{
    SitemapPlan, SitemapRenderError, SitemapWriteError, protocol_sitemap_limits,
    sitemap_shard_number_from_query, sitemap_shard_query_value,
};
use crate::urls::{canonical_home_path, canonical_source_path, sitemap_candidate_path};

const SITEMAP_ARTIFACT_SCHEMA_VERSION: u32 = 2;
const SITEMAP_ARTIFACT_DIR: &str = "sitemap";
const SITEMAP_ARTIFACT_TEMP_PREFIX: &str = ".tmp";
const SITEMAP_ARTIFACT_MANIFEST: &str = "manifest.json";
const SITEMAP_ROOT_FILE: &str = "sitemap.xml";
const SITEMAP_SHARD_FILE_PREFIX: &str = "shard-";
const SITEMAP_SHARD_FILE_SUFFIX: &str = ".xml";
const SITEMAP_CONTENT_TYPE: &str = "application/xml; charset=utf-8";
const SITEMAP_CACHE_CONTROL: &str = "public, max-age=300";

#[derive(Debug, Clone, Default)]
pub(crate) struct SitemapArtifacts {
    current: Arc<RwLock<Option<Arc<SitemapArtifact>>>>,
}

impl SitemapArtifacts {
    pub(crate) fn current(&self) -> Option<Arc<SitemapArtifact>> {
        self.current
            .read()
            .expect("sitemap artifact lock should not be poisoned")
            .clone()
    }

    pub(crate) fn set_current(&self, artifact: Arc<SitemapArtifact>) {
        *self
            .current
            .write()
            .expect("sitemap artifact lock should not be poisoned") = Some(artifact);
    }
}

#[derive(Debug)]
pub(crate) struct SitemapArtifact {
    generation_id: String,
    origin: String,
    lastmod: String,
    root: SitemapArtifactFile,
    shards: BTreeMap<usize, SitemapArtifactFile>,
}

impl SitemapArtifact {
    pub(crate) fn generation_id(&self) -> &str {
        &self.generation_id
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) fn lastmod(&self) -> &str {
        &self.lastmod
    }

    pub(crate) fn file_for_query(
        &self,
        raw_query: Option<&str>,
    ) -> Result<&SitemapArtifactFile, SitemapArtifactLookupError> {
        match sitemap_shard_number_from_query(raw_query) {
            Ok(None) => Ok(&self.root),
            Ok(Some(number)) => self
                .shards
                .get(&number)
                .ok_or(SitemapArtifactLookupError::ShardNotFound),
            Err(_) => Err(SitemapArtifactLookupError::MalformedQuery),
        }
    }
}

#[derive(Debug)]
pub(crate) struct SitemapArtifactFile {
    path: Utf8PathBuf,
    bytes: u64,
    etag: String,
}

impl SitemapArtifactFile {
    pub(crate) async fn serve_response(&self) -> Response {
        match tokio::fs::File::open(&self.path).await {
            Ok(file) => {
                let stream = ReaderStream::new(file);
                let body = Body::from_stream(stream);
                (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, SITEMAP_CONTENT_TYPE.to_owned()),
                        (header::CONTENT_LENGTH, self.bytes.to_string()),
                        (header::CACHE_CONTROL, SITEMAP_CACHE_CONTROL.to_owned()),
                        (header::ETAG, self.etag.clone()),
                    ],
                    body,
                )
                    .into_response()
            }
            Err(error) => {
                tracing::error!(path = %self.path, "failed to open sitemap artifact file: {error:#}");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::RETRY_AFTER, "30")],
                    "sitemap temporarily unavailable",
                )
                    .into_response()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SitemapArtifactLookupError {
    MalformedQuery,
    ShardNotFound,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SitemapArtifactManifest {
    schema_version: u32,
    generation_id: String,
    origin: String,
    lastmod: String,
    root: SitemapArtifactFileManifest,
    shards: Vec<SitemapArtifactShardManifest>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SitemapArtifactFileManifest {
    file: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct SitemapArtifactShardManifest {
    number: usize,
    query_value: String,
    file: String,
    bytes: u64,
    sha256: String,
}

pub(crate) async fn ensure_current_sitemap_artifact(
    config: Arc<AppConfig>,
    search: SearchService,
) -> Result<Arc<SitemapArtifact>> {
    tokio::task::spawn_blocking(move || ensure_current_sitemap_artifact_blocking(config, search))
        .await
        .context("failed to join sitemap artifact task")?
}

pub(crate) fn ensure_current_sitemap_artifact_blocking(
    config: Arc<AppConfig>,
    search: SearchService,
) -> Result<Arc<SitemapArtifact>> {
    let snapshot = search.snapshot();
    build_or_load_sitemap_artifact(config, search, snapshot)
}

fn build_or_load_sitemap_artifact(
    config: Arc<AppConfig>,
    search: SearchService,
    snapshot: ServedGenerationSnapshot,
) -> Result<Arc<SitemapArtifact>> {
    let origin = configured_public_origin(&config)
        .context("public SEO sitemap artifact requires server.public_url")?;
    let generation = snapshot.to_published_generation();
    let final_dir = artifact_dir(&generation.path, &origin);

    match load_sitemap_artifact(&final_dir, &generation, &origin) {
        Ok(artifact) => return Ok(Arc::new(artifact)),
        Err(error) => {
            tracing::info!(
                generation = %generation.path,
                origin,
                "building sitemap artifact because no valid artifact was found: {error:#}"
            );
        }
    }

    build_sitemap_artifact(
        &config,
        &search,
        &snapshot,
        &generation,
        &origin,
        &final_dir,
    )
    .map(Arc::new)
}

fn load_sitemap_artifact(
    artifact_dir: &Utf8Path,
    generation: &PublishedGeneration,
    origin: &str,
) -> Result<SitemapArtifact> {
    let manifest_path = artifact_dir.join(SITEMAP_ARTIFACT_MANIFEST);
    let bytes = fs::read(&manifest_path)
        .with_context(|| format!("failed to read sitemap artifact manifest {manifest_path}"))?;
    let manifest: SitemapArtifactManifest = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to parse sitemap artifact manifest {manifest_path}"))?;

    if manifest.schema_version != SITEMAP_ARTIFACT_SCHEMA_VERSION {
        bail!(
            "unsupported sitemap artifact schema version {}",
            manifest.schema_version
        );
    }
    if manifest.generation_id != generation.manifest.generation_id {
        bail!("sitemap artifact generation id does not match current generation");
    }
    if manifest.origin != origin {
        bail!("sitemap artifact origin does not match configured public origin");
    }
    if manifest.lastmod != sitemap_lastmod(generation.manifest.generated_at) {
        bail!("sitemap artifact lastmod does not match current generation timestamp");
    }

    if manifest.root.file.as_str() != SITEMAP_ROOT_FILE {
        bail!("sitemap artifact root file name is invalid");
    }
    let root = validate_manifest_file(artifact_dir, &manifest.root)?;
    let mut shards = BTreeMap::new();
    for shard in manifest.shards {
        validate_shard_manifest(&shard)?;
        let file = validate_manifest_file(
            artifact_dir,
            &SitemapArtifactFileManifest {
                file: shard.file,
                bytes: shard.bytes,
                sha256: shard.sha256,
            },
        )?;
        if shards.insert(shard.number, file).is_some() {
            bail!("duplicate sitemap artifact shard {}", shard.number);
        }
    }

    Ok(SitemapArtifact {
        generation_id: manifest.generation_id,
        origin: manifest.origin,
        lastmod: manifest.lastmod,
        root,
        shards,
    })
}

fn validate_shard_manifest(shard: &SitemapArtifactShardManifest) -> Result<()> {
    if sitemap_shard_query_value(shard.number).as_deref() != Some(shard.query_value.as_str()) {
        bail!("sitemap artifact shard query value does not match shard number");
    }

    let expected_file = sitemap_shard_file_name(&shard.query_value);
    if shard.file.as_str() != expected_file {
        bail!("sitemap artifact shard file name does not match query value");
    }

    Ok(())
}

fn validate_manifest_file(
    artifact_dir: &Utf8Path,
    file: &SitemapArtifactFileManifest,
) -> Result<SitemapArtifactFile> {
    if file.file.contains('/') || file.file == SITEMAP_ARTIFACT_MANIFEST {
        bail!("invalid sitemap artifact file name {:?}", file.file);
    }

    let path = artifact_dir.join(&file.file);
    let metadata = fs::metadata(&path)
        .with_context(|| format!("failed to read sitemap artifact file metadata {path}"))?;
    if !metadata.is_file() {
        bail!("sitemap artifact path is not a file {path}");
    }
    if metadata.len() != file.bytes {
        bail!("sitemap artifact file size mismatch for {path}");
    }
    let actual_hash = hash_file(&path)?;
    if actual_hash != file.sha256 {
        bail!("sitemap artifact hash mismatch for {path}");
    }

    Ok(SitemapArtifactFile {
        path,
        bytes: file.bytes,
        etag: etag(&file.sha256),
    })
}

fn build_sitemap_artifact(
    config: &AppConfig,
    search: &SearchService,
    snapshot: &ServedGenerationSnapshot,
    generation: &PublishedGeneration,
    origin: &str,
    final_dir: &Utf8Path,
) -> Result<SitemapArtifact> {
    let parent = final_dir
        .parent()
        .context("sitemap artifact dir must have a parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create sitemap artifact parent {parent}"))?;

    let temp_dir = parent.join(format!(
        "{SITEMAP_ARTIFACT_TEMP_PREFIX}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    if temp_dir.exists() {
        fs::remove_dir_all(&temp_dir).with_context(|| {
            format!("failed to remove stale sitemap artifact temp dir {temp_dir}")
        })?;
    }
    fs::create_dir(&temp_dir)
        .with_context(|| format!("failed to create sitemap artifact temp dir {temp_dir}"))?;

    let build_result = build_sitemap_artifact_in_temp(
        config, search, snapshot, generation, origin, final_dir, &temp_dir,
    );
    if build_result.is_err() {
        let _ = fs::remove_dir_all(&temp_dir);
    }

    build_result
}

fn build_sitemap_artifact_in_temp(
    config: &AppConfig,
    search: &SearchService,
    snapshot: &ServedGenerationSnapshot,
    generation: &PublishedGeneration,
    origin: &str,
    final_dir: &Utf8Path,
    temp_dir: &Utf8Path,
) -> Result<SitemapArtifact> {
    let mut paths = vec![canonical_home_path()];
    let lastmod = sitemap_lastmod(generation.manifest.generated_at);
    let candidates = search
        .sitemap_candidates(snapshot)
        .context("failed to collect sitemap candidates")?;
    paths.extend(candidates.iter().map(sitemap_candidate_path));

    for (source_id, source) in &config.sources {
        let Some(ref_id) = source.default_ref.as_deref() else {
            continue;
        };
        if search
            .source_has_indexable_entries(snapshot, source_id, ref_id)
            .with_context(|| {
                format!("failed to check indexable entries for source {source_id} ref {ref_id}")
            })?
        {
            paths.push(canonical_source_path(config, source_id, ref_id));
        }
    }

    let plan = SitemapPlan::with_sitemap_index_lastmod(
        origin.to_owned(),
        paths,
        Some(lastmod.clone()),
        protocol_sitemap_limits(),
    )
    .map_err(sitemap_render_error)?;
    let root = write_sitemap_root(temp_dir, &plan)?;
    let mut shard_manifests = Vec::new();

    let shard_infos = plan.shard_infos();
    if shard_infos.len() > 1 {
        for shard in shard_infos {
            let filename = sitemap_shard_file_name(&shard.query_value);
            let written = write_sitemap_shard(temp_dir, &filename, &plan, shard.number)?;
            shard_manifests.push(SitemapArtifactShardManifest {
                number: shard.number,
                query_value: shard.query_value,
                file: filename,
                bytes: written.bytes,
                sha256: written.sha256,
            });
        }
    }

    let manifest = SitemapArtifactManifest {
        schema_version: SITEMAP_ARTIFACT_SCHEMA_VERSION,
        generation_id: generation.manifest.generation_id.clone(),
        origin: origin.to_owned(),
        lastmod,
        root: SitemapArtifactFileManifest {
            file: SITEMAP_ROOT_FILE.to_owned(),
            bytes: root.bytes,
            sha256: root.sha256,
        },
        shards: shard_manifests,
    };
    write_json_file(
        &temp_dir.join(SITEMAP_ARTIFACT_MANIFEST),
        &serde_json::to_vec_pretty(&manifest)
            .context("failed to serialize sitemap artifact manifest")?,
    )?;

    if final_dir.exists() {
        fs::remove_dir_all(final_dir)
            .with_context(|| format!("failed to remove old sitemap artifact dir {final_dir}"))?;
    }
    fs::rename(temp_dir, final_dir)
        .with_context(|| format!("failed to publish sitemap artifact {temp_dir} to {final_dir}"))?;

    load_sitemap_artifact(final_dir, generation, origin)
}

struct WrittenFile {
    bytes: u64,
    sha256: String,
}

fn write_sitemap_root(dir: &Utf8Path, plan: &SitemapPlan) -> Result<WrittenFile> {
    write_generated_file(dir, SITEMAP_ROOT_FILE, |writer| {
        plan.write_root(writer).map_err(anyhow::Error::from)
    })
}

fn write_sitemap_shard(
    dir: &Utf8Path,
    filename: &str,
    plan: &SitemapPlan,
    number: usize,
) -> Result<WrittenFile> {
    write_generated_file(dir, filename, |writer| {
        plan.write_shard(number, writer)
            .map_err(sitemap_write_error)
    })
}

fn write_json_file(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path).with_context(|| format!("failed to create {path}"))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {path}"))?;
    file.sync_all()
        .with_context(|| format!("failed to sync {path}"))?;

    Ok(())
}

fn write_generated_file(
    dir: &Utf8Path,
    filename: &str,
    write: impl FnOnce(&mut HashingFileWriter) -> Result<()>,
) -> Result<WrittenFile> {
    let path = dir.join(filename);
    let mut writer = HashingFileWriter::create(&path)?;
    write(&mut writer).with_context(|| format!("failed to write {path}"))?;
    writer.finish()
}

struct HashingFileWriter {
    path: Utf8PathBuf,
    inner: BufWriter<File>,
    hasher: Sha256,
    bytes: u64,
}

impl HashingFileWriter {
    fn create(path: &Utf8Path) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("failed to create {path}"))?;

        Ok(Self {
            path: path.to_owned(),
            inner: BufWriter::new(file),
            hasher: Sha256::new(),
            bytes: 0,
        })
    }

    fn finish(mut self) -> Result<WrittenFile> {
        self.inner
            .flush()
            .with_context(|| format!("failed to flush {}", self.path))?;
        self.inner
            .get_ref()
            .sync_all()
            .with_context(|| format!("failed to sync {}", self.path))?;

        Ok(WrittenFile {
            bytes: self.bytes,
            sha256: hex::encode(self.hasher.finalize()),
        })
    }
}

impl Write for HashingFileWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.bytes += written as u64;

        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn hash_file(path: &Utf8Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("failed to open {path}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read {path}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn artifact_dir(generation_path: &Utf8Path, origin: &str) -> Utf8PathBuf {
    generation_path
        .join(SITEMAP_ARTIFACT_DIR)
        .join(&hash_bytes(origin.as_bytes())[..16])
}

fn sitemap_shard_file_name(query_value: &str) -> String {
    format!("{SITEMAP_SHARD_FILE_PREFIX}{query_value}{SITEMAP_SHARD_FILE_SUFFIX}")
}

fn etag(sha256: &str) -> String {
    format!(r#""sitemap-{sha256}""#)
}

pub(crate) fn sitemap_lastmod(generated_at: OffsetDateTime) -> String {
    generated_at
        .format(&Rfc3339)
        .expect("RFC3339 formatting should not fail for OffsetDateTime")
}

fn sitemap_render_error(error: SitemapRenderError) -> anyhow::Error {
    anyhow::anyhow!("failed to render sitemap artifact: {error:?}")
}

fn sitemap_write_error(error: SitemapWriteError) -> anyhow::Error {
    match error {
        SitemapWriteError::Render(error) => sitemap_render_error(error),
        SitemapWriteError::Io(error) => error.into(),
    }
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock should be after Unix epoch")
        .as_nanos()
}
