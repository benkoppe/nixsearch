# Nix Search Rust Project Plan

This is a rough architectural plan for a future Rust project inspired by `nixos-search`, `searchix`, and `NuschtOS-search`. It is intentionally not a strict specification. Future agents should treat it as guidance, revise it when better decisions appear, and preserve the main design goals: flexibility, local indexing, clear separation between backend/search/frontend, and broad support for Nix ecosystem data.

## Goals

- Build a search system for Nix packages, NixOS options, Home Manager options, nix-darwin options, flake outputs, and arbitrary module-based projects.
- Keep the frontend sufficiently separated so the same backend can support a website, CLI, TUI, editor plugin, or static exporter.
- Avoid requiring paid services such as hosted Elasticsearch/OpenSearch.
- Support multiple repositories, datasets, and refs/branches in one deployment.
- Be more flexible than `searchix` by making refs and import strategies first-class configuration concepts.
- Support known common cases with presets, but allow custom projects such as `hjem`, NixVim, Nixidy, organization module collections, etc.
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
- Supporting `optionsPrefix`, `urlPrefix`, `specialArgs`, and `overrideEvalModulesArgs` makes arbitrary module projects much easier.

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
  api/           HTTP/search API, frontend-agnostic
  cli/           commands: produce, consume, index, update, serve, inspect
  web/           optional website shell/assets later
```

The key rule:

> Frontends should consume a stable search/detail API and should not know how Nix data is fetched, parsed, or indexed.

This makes the backend reusable by a website, CLI, TUI, editor integration, or static exporter.

## Core conceptual model

Keep these concepts distinct.

### Project

A logical collection or deployment namespace.

Examples:

- `nixpkgs`
- `nixos`
- `home-manager`
- `nix-darwin`
- `hjem`
- `my-org-options`

A project may contain multiple datasets and refs.

### Dataset

A searchable logical collection inside a project.

Examples:

- `packages`
- `nixos-options`
- `home-manager-options`
- `darwin-options`
- `apps`
- `services`
- `hjem-options`

A dataset has:

- stable ID
- display name
- kind: `packages`, `options`, `apps`, `services`, or `mixed`
- repository/link metadata
- import strategy
- available facets

### Ref

A concrete version of a dataset.

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
project_id/dataset_id/ref_id/kind/name
```

This allows one deployment to contain, for example:

- `nixpkgs/packages/nixos-unstable`
- `nixpkgs/packages/nixos-25.05`
- `nixpkgs/nixos-options/nixos-unstable`
- `home-manager/options/master`
- `nix-darwin/options/master`

Future agents may decide whether physical Tantivy indexes should be global, per project, per dataset, or per ref. The logical model should support all of these.

## Configuration design

Use a declarative config format such as TOML, with optional Nix-generated config later. Prefer `figment` for configuration loading so settings can be layered from defaults, config files, environment variables, and CLI overrides.

Recommended config source order:

```text
built-in defaults
  -> nix-search.toml
  -> NIX_SEARCH_* environment variables
  -> CLI overrides
```

The server/importer should generally be configured with projects, datasets, refs, and producers rather than direct paths to `options.json` or `packages.json`. Direct file paths should still be supported as a low-level/debug producer.

Conceptual example:

```toml
[projects.nixpkgs]
name = "Nixpkgs"

[[projects.nixpkgs.datasets]]
id = "packages"
name = "Nix Packages"
kind = "packages"

[[projects.nixpkgs.datasets.refs]]
id = "unstable"
ref = "github:NixOS/nixpkgs/nixos-unstable"
producer = "channel-packages-json"

[[projects.nixpkgs.datasets.refs]]
id = "25.05"
ref = "github:NixOS/nixpkgs/nixos-25.05"
producer = "channel-packages-json"

[[projects.nixpkgs.datasets]]
id = "nixos-options"
name = "NixOS Options"
kind = "options"

[[projects.nixpkgs.datasets.refs]]
id = "unstable"
ref = "github:NixOS/nixpkgs/nixos-unstable"
producer = "nix-build-options-json"
attribute = "options"
import_path = "nixos/release.nix"
output_path = "share/doc/nixos/options.json"
```

Arbitrary module project example:

```toml
[projects.hjem]
name = "hjem"

[[projects.hjem.datasets]]
id = "options"
name = "hjem Options"
kind = "options"

[[projects.hjem.datasets.refs]]
id = "main"
ref = "github:feel-co/hjem"
producer = "eval-modules"
modules_attr = "nixosModules.default"
options_prefix = "hjem"
url_prefix = "https://github.com/feel-co/hjem/blob/main/"
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
  -> Resolve project/dataset/ref
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
artifact_url = "s3://my-bucket/nix-search/artifacts"
```

Artifacts are the boundary between production and ingestion. They allow caching, debugging, reindexing without rerunning Nix, CI-produced artifacts, and clear separation between producer failures and parser/indexer failures.

Recommended artifact keys:

```text
artifacts/{project}/{dataset}/{ref}/latest/options.json
artifacts/{project}/{dataset}/{ref}/latest/packages.json
artifacts/{project}/{dataset}/{ref}/latest/meta.json
artifacts/{project}/{dataset}/{ref}/revisions/{revision}/options.json
artifacts/{project}/{dataset}/{ref}/revisions/{revision}/packages.json
artifacts/{project}/{dataset}/{ref}/revisions/{revision}/meta.json
```

Keep `latest` for convenience, but prefer revision-addressed or content-addressed storage for reproducibility.

Metadata should include:

- project
- dataset
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
- `modules_attr`
- `specialArgs`
- `overrideEvalModulesArgs`
- `optionsPrefix`
- `urlPrefix`

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
    project: String,
    dataset: String,
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
- `project`
- `dataset`
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

- project
- dataset
- ref
- kind
- option set
- option parent
- package set
- platform
- license
- maintainer

## API design

Build a frontend-agnostic API.

Possible endpoints:

```text
GET /api/projects
GET /api/datasets
GET /api/refs
GET /api/search?q=git&project=nixpkgs&dataset=packages&ref=unstable
GET /api/document/{id}
GET /api/suggest?q=git
GET /api/status
```

Search responses should include:

- stable document ID
- kind
- project/dataset/ref
- title/name
- score
- short summary
- relevant metadata
- highlights if available
- facets

Do not bake HTML into API responses. The web UI should render from structured data.

## CLI design

Provide a CLI that can run without the web server.

Potential commands:

```text
nix-search produce
nix-search consume
nix-search index
nix-search update
nix-search serve
nix-search search "programs.git.enable"
nix-search inspect-source nixos-options unstable
nix-search list-projects
nix-search list-datasets
nix-search list-refs
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
- project/dataset/ref revisions
- import start/end time
- document counts
- warnings/errors
- artifact paths
- generator version

## Source links

Preserve and normalize source links.

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

- Keep UI separate from backend crates.
- Use the API as the contract.
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
- API response stability
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
13. Add project/dataset/ref config model.
14. Add `NixBuildOptionsProducer`.
15. Add `ChannelPackagesJsonProducer`.
16. Add package parser for nixpkgs `packages.json`.
17. Add normalized `PackageDoc`.
18. Add package indexing/search.
19. Add Axum API.
20. Add index generations and metadata.
21. Add source link generation.
22. Add `programs.sqlite` enrichment.
23. Add `EvalModulesProducer` inspired by NuschtOS.
24. Add built-in presets for nixpkgs/NixOS/Home Manager/nix-darwin.
25. Start serious frontend work.

This order gives future agents a working vertical slice early while preserving flexibility.

## Important open questions

Future agents should revisit these with experiments:

- One global Tantivy index or multiple indexes per project/dataset/ref?
- How much should custom Nix eval be configured in TOML vs Nix?
- Should the project expose a Nix library similar to NuschtOS `mkSearch`?
- How should schema migrations work for existing indexes?
- How much markdown should be rendered at ingestion time vs frontend time?
- Should full documents be stored in Tantivy, SQLite, or separate JSON blobs?
- Should the web API support federated/multiple index backends?
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

The main improvement is a more general model built around projects, datasets, and refs, plus a clean frontend-agnostic API and local Tantivy indexing.
