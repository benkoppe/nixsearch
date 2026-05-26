# nixsearch Rust Project Plan

This is a rough architectural plan for a future Rust project inspired by `nixos-search`, `searchix`, and `NuschtOS-search`. It is intentionally not a strict specification. Future agents should treat it as guidance, revise it when better decisions appear, and preserve the main design goals: flexibility, local indexing, clear separation between backend/search/frontend, and broad support for Nix ecosystem data.

## Goals

- Build a search system for Nix packages, NixOS options, Home Manager options, nix-darwin options, flake outputs, and arbitrary module-based sources.
- Keep search/indexing/domain logic separate from presentation so the same core can support a Datastar website, CLI, TUI, editor plugin, or static exporter.
- Avoid requiring paid services such as hosted Elasticsearch/OpenSearch.
- Support multiple repositories, sources, and refs/branches in one deployment.
- Be more flexible than `searchix` by making refs and import strategies first-class configuration concepts.
- Support known common cases with presets, but allow custom sources such as `hjem`, NixVim, Nixidy, organization module collections, etc.
- Preserve rich metadata: source links, revisions, declarations, package positions, licenses, maintainers, platforms, programs, manpages, and option defaults/examples.

## Lessons from reference projects

### `nixos-search`

Useful parts:

- `references/nixos-search/flake-info/`
- `references/nixos-search/frontend/`

Key lessons:

- Strong separation between data extraction and frontend search UI is valuable.
- `flake-info` is a useful Rust reference for normalizing flake and nixpkgs data.
- nixpkgs package data can often be obtained from channel artifacts like:

  ```text
  https://channels.nixos.org/nixos-unstable/packages.json.br
  ```

- Options are generally obtained by evaluating Nix expressions or building `options.json`.
- A normalized document schema helps support packages, apps, options, and services in a single search experience.

Things to avoid:

- Requiring Elasticsearch/OpenSearch.
- Tying the data model too tightly to one hosted search backend.
- Making nixpkgs channels so special that arbitrary module repositories become awkward.

### `searchix`

Useful parts:

- `references/searchix/defaults.toml`
- `references/searchix/internal/config/structs.go`
- `references/searchix/internal/fetcher/*.go`
- `references/searchix/internal/importer/*.go`
- `references/searchix/internal/index/*.go`

Key lessons:

- The source/fetcher/importer split is very useful.
- Local indexing is practical; `searchix` uses Bleve instead of Elasticsearch.
- It already supports NixOS, nixpkgs, nix-darwin, Home Manager, and NUR.
- It enriches package search with `programs.sqlite` and manpage data.
- Ranking needs hand-tuned boosts:
  - exact attr/name match very high
  - prefix match high
  - ngram/fuzzy match medium
  - description lower
  - `mainProgram`/`programs` high for packages
- Facets are useful for option sets, parent options, package sets, platforms, etc.

Things to improve:

- Refs/branches should be first-class rather than implicit in a source.
- Fetch/import behavior should be more configurable and pluggable.
- Search API and rendering should be separated more cleanly.
- Use a Rust-native local search engine.

### `NuschtOS-search`

Useful parts:

- `references/NuschtOS-search/flake.nix`
- `references/NuschtOS-search/nix/wrapper.nix`

Key lessons:

- Static generation is a useful deployment mode.
- Its `mkSearch`/`mkMultiSearch` API is excellent inspiration for arbitrary option scopes.
- A scope can be backed by either:
  - modules evaluated with `lib.evalModules`
  - a pre-generated `options.json`
- Supporting `urlPrefix`, `specialArgs`, and `overrideEvalModulesArgs` makes arbitrary module sources much easier.

Things to avoid:

- Being option-search only.
- Making static deployment the only architecture.
- Coupling search behavior too tightly to a frontend bundle.

## Proposed Rust workspace

Suggested structure:

```text
crates/
  core/          shared domain types, errors
  config/        Figment-based typed configuration and validation
  store/         artifact storage built on object_store
  source/        source/ref resolution, producers, Nix evaluators
  ingest/        artifact consumers and parsers into normalized documents
  index/         Tantivy schema, ranking, facets, index lifecycle
  web/           Datastar-powered web UI and HTTP routes
  cli/           commands: produce, consume, index, update, serve, inspect
```

The key rule:

> Presentation should depend on stable search/indexing services and should not know how Nix data is fetched, parsed, or indexed.

The website should be built with Datastar. Keep the core crates reusable so CLI, TUI, editor integrations, and static exporters can call the same search/indexing logic without depending on the web UI.

## Core conceptual model

Keep these concepts distinct.

### Source

A top-level searchable collection.

Examples:

- `nixpkgs` — Nix packages
- `nixos` — NixOS options
- `home-manager` — Home Manager options
- `nix-darwin` — nix-darwin options
- `hjem` — hjem options
- `fixtures` — local/debug data

A source has:

- stable ID
- display name
- kind: `packages`, `options`, `apps`, `services`, or `mixed`
- repository/link metadata
- import strategy
- available refs
- effective default ref
- available facets

A source always has an effective default ref when it has at least one ref. If `default_ref` is configured, it must match a configured ref. If it is omitted, the first configured ref is used. Single-ref sources therefore do not need explicit default configuration.

### Ref

A concrete version of a source.

Examples:

- `nixos-unstable`
- `nixos-25.05`
- `master`
- `main`
- commit SHA
- flake lock node

Refs should be first-class. Branches do not need a special primitive; they can be represented as refs.

Document IDs should include all relevant identity dimensions:

```text
source_id/ref_id/kind/name
```

This allows one deployment to contain, for example:

- `nixpkgs/nixos-unstable/package/git`
- `nixpkgs/nixos-25.05/package/git`
- `nixos/nixos-unstable/option/programs.git.enable`
- `home-manager/master/option/programs.git.enable`
- `nix-darwin/master/option/system.defaults.dock.autohide`

Future agents may decide whether physical Tantivy indexes should be global, per source, or per ref. The logical model should support all of these.

## Configuration design

Use a declarative config format such as TOML, with optional Nix-generated config later. Prefer `figment` for configuration loading so settings can be layered from defaults, config files, environment variables, and CLI overrides.

Recommended config source order:

```text
built-in defaults
  -> nixsearch.toml
  -> NIXSEARCH_* environment variables
  -> CLI overrides
```

The server/importer should generally be configured with sources, refs, and producers rather than direct paths to `options.json` or `packages.json`. Direct file paths should still be supported as a low-level/debug producer.

Conceptual example:

```toml
[sources.nixpkgs]
name = "Nixpkgs"
preset = "nixpkgs-packages"
ref = "nixos-unstable"

[sources.nixos]
name = "NixOS Options"
preset = "nixos-options"
ref = "nixos-unstable"
```

Explicit source example:

```toml
[sources.nixpkgs]
name = "Nixpkgs"
kind = "packages"

[[sources.nixpkgs.refs]]
id = "unstable"

[sources.nixpkgs.refs.producer]
type = "channel-packages-json"
channel = "nixos-unstable"
```

Arbitrary module source example:

```toml
[sources.hjem]
name = "hjem Options"
kind = "options"

[[sources.hjem.refs]]
id = "main"

[sources.hjem.refs.source_links]
type = "github"
owner = "feel-co"
repo = "hjem"
revision = "main"

[sources.hjem.refs.producer]
type = "eval-modules"
ref = "github:feel-co/hjem"

[[sources.hjem.refs.producer.modules]]
type = "flake-attr"
attr = "nixosModules.default"
```

Prefer built-in presets for common cases, but ensure all presets expand to regular config:

- `preset = "nixpkgs-packages"`
- `preset = "nixos-options"`
- `preset = "home-manager-options"`
- `preset = "nix-darwin-options"`
- `preset = "eval-modules"`

## Pipeline

Use a producer/consumer pipeline:

```text
Figment config
  -> Resolve source/ref
  -> Producer fetches/evaluates source data
  -> Artifact store writes raw artifacts and metadata
  -> Consumer reads artifact
  -> Parser normalizes documents
  -> Enrich documents
  -> Index documents
  -> Atomically publish index generation
```

Each stage should be independently testable. The current low-level `options.json -> index` flow is still valuable, but it should be treated as one producer/consumer path rather than the long-term primary interface.

## Artifact storage

Use an artifact abstraction backed by `object_store`. The default backend should be local filesystem storage, but the abstraction should allow S3/GCS/Azure-compatible stores later.

Example config:

```toml
[data]
artifact_url = "file://./data/artifacts"
index_dir = "./data/indexes"
```

Remote artifact storage should be possible later:

```toml
[data]
artifact_url = "s3://my-bucket/nixsearch/artifacts"
```

Artifacts are the boundary between production and ingestion. They allow caching, debugging, reindexing without rerunning Nix, CI-produced artifacts, and clear separation between producer failures and parser/indexer failures.

Recommended artifact keys:

```text
artifacts/{source}/{ref}/latest/options.json
artifacts/{source}/{ref}/latest/packages.json
artifacts/{source}/{ref}/latest/meta.json
artifacts/{source}/{ref}/revisions/{revision}/options.json
artifacts/{source}/{ref}/revisions/{revision}/packages.json
artifacts/{source}/{ref}/revisions/{revision}/meta.json
```

Keep `latest` for convenience, but prefer revision-addressed or content-addressed storage for reproducibility.

Metadata should include:

- source
- ref
- artifact kind
- producer name/config hash
- resolved revision
- content hash
- produced timestamp
- source URL/ref
- artifact size
- warnings/errors

Do not store live Tantivy indexes in `object_store` initially. Tantivy should open local filesystem indexes. Later, completed index generations can optionally be archived to object storage as snapshots.

## Producers and consumers

Use producer/consumer terminology rather than fetcher/importer where possible.

A producer turns a configured source/ref into a raw artifact:

```text
source/ref -> options.json/packages.json/flake-info.json/etc
```

A consumer turns an artifact into normalized documents:

```text
artifact -> SearchDocument stream
```

This separation is important. For example, an `options.json` consumer should not care whether the file came from `nix-build`, `evalModules`, a download, S3, or a local fixture.

Suggested producer types:

### `channel-packages-json`

Downloads nixpkgs channel package JSON, usually `packages.json.br`.

Useful for nixpkgs packages.

### `nix-build-options-json`

Builds an `options.json` using `nix-build`, similar to `searchix`.

Useful for:

- NixOS options
- Home Manager options
- nix-darwin options
- other projects exposing an options JSON derivation

### `nix-eval-expression`

Runs a Nix expression and expects JSON output.

Useful as a flexible advanced option.

### `flake-output`

Extracts flake packages/apps/options, inspired by `flake-info`.

Useful for arbitrary flakes exposing packages/apps.

### `download`

Downloads or reads already-generated artifacts:

- `options.json`
- `packages.json`
- `revision`

Useful for static/generated sources such as NUR-like repositories.

### `eval-modules`

Inspired by NuschtOS. Evaluates arbitrary modules via `lib.evalModules` and `nixosOptionsDoc`.

Should support:

- `modules`
- explicit `inputs`
- `specialArgs`
- `overrideEvalModulesArgs`

### `custom-command`

Escape hatch. Runs a command that outputs a known JSON schema.

This should be treated as advanced/unsafe but useful for flexibility.

## Consumers and parsers

Initial consumers:

- `OptionsJsonConsumer`
- `PackagesJsonConsumer`
- later `FlakeInfoConsumer`

Support known raw formats:

- nixpkgs `packages.json`
- NixOS `options.json`
- Home Manager `options.json`
- nix-darwin `options.json`
- flake package/app info from `flake-info`-style extraction
- custom normalized JSON

Use streaming parsing for large files.

Important Rust crates to consider:

- `serde`
- `serde_json`
- `thiserror`
- `anyhow`
- `tokio`
- `reqwest`
- `figment = 0.10.19`
- `object_store = 0.13.2`
- `tantivy = 0.26.1`
- `axum = 0.8.9`
- compression crates as needed for `.br`/`.zst`/`.gz`

Per `GUIDELINES.md`, prefer using dependencies rather than reinventing functionality.

## Normalized document model

Use a single enum with shared common metadata.

Conceptually:

```rust
struct CommonDoc {
    id: String,
    source: String,
    ref_id: String,
    kind: DocumentKind,
    name: String,
    source_repo: Option<Repo>,
    revision: Option<String>,
    imported_at: DateTime,
}

enum SearchDocument {
    Option(OptionDoc),
    Package(PackageDoc),
    App(AppDoc),
    Service(ServiceDoc),
}
```

### Option document fields

Include, when available:

- option name
- `loc` segments
- parent options
- top-level option set
- type
- description
- default
- example
- declarations
- related packages
- read-only/internal/visible flags
- source links

### Package document fields

Include, when available:

- attribute path
- package set
- `pname`
- version
- description
- long description
- platforms
- licenses, including compound expressions if possible
- maintainers
- teams
- homepage
- source position/definition link
- outputs
- default output
- main program
- programs from `programs.sqlite`
- broken/insecure/unfree flags
- modular services

### App document fields

Include, when available:

- app attr path
- platforms
- app type
- binary path

### Service document fields

Include, when available:

- option-like fields
- service package
- service module
- associated packages

Store the full normalized document as JSON for result rendering/detail pages, while indexing selected fields separately.

## Indexing

Use Tantivy to avoid hosted search services.

Important requirements:

- local on-disk index
- schema versioning
- atomic generation swap
- fast detail lookup by ID
- facets
- field boosts
- exact fields
- prefix/ngram fields
- full-text fields

Suggested indexed fields:

- `id`
- `source`
- `ref`
- `kind`
- `name_exact`
- `name_text`
- `name_ngram`
- `attr_exact`
- `attr_text`
- `description`
- `option_set`
- `option_parent`
- `package_set`
- `platform`
- `program`
- `main_program`
- `license`
- `maintainer`
- `stored_json`

Future agents should tune schema and analyzers with real query examples.

## Search ranking

Ranking should be tested against realistic queries.

Initial goals:

- Exact attr/name matches dominate.
- Prefix attr/name matches rank high.
- Ngram/fuzzy matches are useful but lower.
- Description matches should not overpower name matches.
- Package `mainProgram` exact match should be very high.
- Package `programs` exact match should be high.
- Option parent/name matches should prefer direct, shorter options when appropriate.
- Multi-word descriptions should help discovery but not pollute exact package/option lookup.

Example important queries:

- `git`
  - packages: `git`, `gitMinimal`, `gitFull`
  - options: `programs.git.enable`, `programs.git.package`
- `programs.git.enable`
- `services.nginx.enable`
- `firefox`
- `home-manager git`
- `launchd`
- `mainProgram`

Support filters/facets:

- source
- ref
- kind

Search should use default refs by default. If no source/ref filter is provided, search all sources at their default refs. If a source is selected but no ref is selected, search that source's default ref. Searching all refs should be an explicit advanced/debug option because refs such as stable and unstable usually contain heavily overlapping documents.

- option set
- option parent
- package set
- platform
- license
- maintainer

## Web design

Build the website with [Datastar](https://data-star.dev/). Datastar is a good fit because search UI interactions can be handled with server-rendered HTML fragments, signals, and SSE updates without building a heavy client-side app.

The web server can still use Axum internally as the Rust HTTP framework, but the primary web contract should be Datastar-oriented HTML/fragments rather than a frontend-agnostic JSON API.

Potential routes:

```text
GET /                         main search page
GET /search?q=git&source=... returns a page or Datastar fragment
GET /document/{id}            detail page or detail fragment
GET /status                   human-readable status
GET /-/health                 simple health check
```

Search/page rendering should include:

- stable document links
- kind
- source/ref
- title/name
- score if useful for debugging
- short summary
- relevant metadata
- highlights if available
- facets/groups/filters

Keep search functionality in reusable crates. The Datastar web layer should call shared search services rather than embedding indexing/search logic directly. A small JSON/debug API can be added later if useful, but it should not drive the primary frontend design.

## CLI design

Provide a CLI that can run without the web server.

Potential commands:

```text
nixsearch produce
nixsearch consume
nixsearch index
nixsearch update
nixsearch serve
nixsearch search "programs.git.enable"
nixsearch inspect-source nixos unstable
nixsearch list-sources
nixsearch list-refs
```

Low-level file commands such as `index-options --options-json ./options.json` should remain available for tests/debugging, but internally they should eventually go through an `ExistingFileProducer` and the normal artifact consumer path.

The CLI is important for testing the backend before a serious frontend exists.

## Index lifecycle

Use generation-based index directories:

```text
data/
  artifacts/
  indexes/
    generation-000001/
    generation-000002/
  current -> generation-000002
  metadata.json
```

Benefits:

- failed imports do not corrupt the current index
- atomic index swaps
- rollback
- pruning old generations
- easy debugging

Metadata should track:

- schema version
- config hash
- source/ref revisions
- import start/end time
- document counts
- warnings/errors
- artifact paths
- generator version

## Source links

Preserve and normalize source links.

Source links should be resolved as cross-producer enrichment from raw declarations and positions. The raw parsed document should preserve the original declaration or package `meta.position`; configured source-link metadata on a source/ref should derive renderable URLs. This follows searchix's repository-based declaration/definition linking and NüschtOS's `urlPrefix` flexibility, while avoiding destructive artifact mutation.

Source-link configuration such as `urlPrefix` is inspired by NüschtOS, but it should be treated as
cross-producer enrichment rather than as part of artifact production. Producers and consumers should preserve raw declarations/positions from `options.json` and `packages.json`; a later resolver should derive renderable URLs from repo/ref metadata, URL templates, strip prefixes, and revisions. This avoids regenerating artifacts just because source-link policy changes.

Producers should populate artifact metadata revisions when practical. Source-link resolution should prefer exact artifact/document revisions over configured branch names, falling back to configured revisions only when no exact revision is known.

Support at least GitHub initially:

```text
https://github.com/{owner}/{repo}/blob/{revision}/{path}#L{line}
```

Design `Repo` so other hosts can be added:

- GitHub
- GitLab
- SourceHut
- Codeberg/Gitea
- arbitrary URL template

Declarations can be absolute Nix store paths, relative source paths, or already-formed links. Parsers should be defensive.

## Enrichment

Add enrichment as optional stages.

Useful enrichers:

- `programs.sqlite` mapping packages to executable names
- manpage URL mapping
- package-to-module-service mapping
- source link normalization
- markdown rendering/sanitization
- license normalization
- option prefixing

Enrichment failures should usually warn, not abort the whole import, unless configured otherwise.

## Static deployment mode

Do not optimize for this first, but keep it possible.

A future static exporter could produce:

```text
static/
  index files or compressed search chunks
  documents chunks
  metadata.json
  frontend assets
```

NuschtOS proves this is valuable for GitHub Pages or simple deployments. However, the primary architecture should start with a backend + Tantivy index because it is more flexible for packages and multi-ref search.

## Frontend guidance

Frontend work should come after backend/search fundamentals.

Principles:

- Build the web frontend with Datastar.
- Keep Datastar/web rendering separate from core indexing/search crates.
- Prefer server-rendered HTML and Datastar fragments/signals over a heavy client-side SPA.
- Make source/ref switching easy and obvious.
- Avoid hiding important filters.
- Result pages should be linkable and stable.
- Make keyboard navigation good.
- Avoid copying NuschtOS/searchix UI decisions blindly.

## Testing strategy

Use real fixture data from small generated `options.json` and package snippets.

Test areas:

- config parsing
- source/ref identity
- options JSON parsing
- packages JSON parsing
- source link generation
- document normalization
- Tantivy indexing
- ranking behavior on known queries
- Datastar route/fragment rendering stability
- index generation swap/rollback

Ranking tests should assert relative ordering for key queries, not exact scores.

## Initial implementation order

Recommended vertical-slice order:

1. Create Rust workspace.
2. Implement core domain types.
3. Implement parser for existing `options.json`.
4. Implement normalized `OptionDoc`.
5. Implement a minimal Tantivy index for options.
6. Implement CLI command to index an `options.json` file.
7. Implement CLI search command.
8. Add `crates/config` with Figment-based typed config loading.
9. Add `crates/store` with artifact storage backed by `object_store`.
10. Add artifact types: `ArtifactKind`, `ArtifactRef`, `ArtifactMetadata`.
11. Add producer/consumer traits.
12. Add `ExistingFileProducer` and refactor `index-options` to use the artifact path internally.
13. Add source/ref config model.
14. Add `NixBuildOptionsProducer`.
15. Add `ChannelPackagesJsonProducer`.
16. Add package parser for nixpkgs `packages.json`.
17. Add normalized `PackageDoc`.
18. Add package indexing/search.
19. Add index generations and manifests.
20. Add source link generation.
21. Add `programs.sqlite` enrichment.
22. Add `EvalModulesProducer` inspired by NuschtOS.
23. Add built-in presets for nixpkgs/NixOS/Home Manager/nix-darwin.
24. Add Datastar-powered web UI/routes.
25. Add optional JSON/debug endpoints only if useful.

This order gives future agents a working vertical slice early while preserving flexibility.

## Important open questions

Future agents should revisit these with experiments:

- One global Tantivy index or multiple indexes per source/ref?
- How much should custom Nix eval be configured in TOML vs Nix?
- Should the project expose a Nix library similar to NuschtOS `mkSearch`?
- How should schema migrations work for existing indexes?
- How much markdown should be rendered at ingestion time vs frontend time?
- Should full documents be stored in Tantivy, SQLite, or separate JSON blobs?
- Should the Datastar web layer expose any stable JSON/debug endpoints, or keep JSON private/internal?
- What is the best analyzer/tokenizer setup for dotted Nix attribute paths?

## Non-goals for early versions

- Perfect frontend design.
- Distributed indexing.
- Hosted search service support.
- Real-time updates.
- Supporting every possible flake convention immediately.
- Complex auth/multi-user features.

## Summary

The project should combine the best parts of the references:

- From `nixos-search`: Rust extraction ideas and normalized Nix ecosystem document modeling.
- From `searchix`: practical local indexing, fetch/import/index separation, and multi-source support.
- From `NuschtOS-search`: flexible arbitrary module option scopes and static-generation inspiration.

The main improvement is a more general model built around sources and refs, plus reusable search/indexing crates, a Datastar-powered web UI, and local Tantivy indexing.
