//! Build-time service-route scanning and registration generation.
//!
//! # Who compiles this module tree, and why it lives next to `build.rs`
//!
//! * `build.rs` (host-only build script) resolves `mod build_support;` to this file.
//! * `tests/discovery.rs` compiles the **same source** with
//!   `#[path = "../build_support/mod.rs"] mod build_support;`. A build script is not
//!   linked as an rlib, so an integration test otherwise cannot assert on the rules at
//!   all - they would only be observable through a generated file.
//! * `src/` never touches it: scanning is build-time only, so neither filesystem walking
//!   nor the `ignore` crate can leak into the `wasm32-unknown-unknown` artifact (R4).
//!
//! A separate internal crate was rejected on purpose: it would add a workspace member
//! (manifest churn) and a published-internal crate, for zero gain - the `#[path]`
//! include keeps one source of truth with no build-graph change.
//!
//! # What is generated
//!
//! `$OUT_DIR/service_routes.rs` (see [`GENERATED_FILE_NAME`]) contains module-scope items
//! only: one absolute `#[path]` module per discovered route file, then
//! `pub fn register_routes(router: axum::Router<crate::server::AppState>) -> axum::Router<crate::server::AppState>`
//! and `pub fn route_metadata() -> Vec<crate::server::EndpointMeta>` (R3). The **crate root**
//! includes it (`src/lib.rs`), which is the only place the include and the
//! `crate::register_routes` call can be kept in step - the generated signatures use
//! absolute `crate::server::AppState` paths, so the include location and the call path are
//! a pair.
//!
//! ```text
//! include!(concat!(env!("OUT_DIR"), "/service_routes.rs"));
//! ```
//!
//! # Which files are route files
//!
//! A file is a route source exactly when its name is `<non-empty stem>-route.rs`
//! ([`ROUTE_FILE_SUFFIX`], case-sensitive, no `.test.`/`.spec.` stem segment, decided by
//! [`matching::is_route_file_name`]) **and** it sits inside an `endpoint`/`endpoints`
//! directory tree ([`ENDPOINT_DIR_NAMES`], [`matching::inside_endpoint_tree`]). Nothing
//! else in such a tree is ever discovered: a `<stem>-core.rs` implementation file, a
//! `mod.rs`, and every pre-rename `service-route*` spelling are ordinary files as far as the
//! generator is concerned, which is what lets one resource ship as a route file plus any
//! number of implementation files without mounting or listing itself twice.
//!
//! # Route file contract (what a discovered file must provide)
//!
//! One `crate::service_routes!` invocation ([`ROUTE_MACRO_PATH`], defined in
//! `src/server/metadata.rs`) that lists the file's endpoints. The macro emits the two items
//! the generator relies on, from the same entries:
//!
//! ```text
//! crate::service_routes! {
//!     GET "/v1/services/health" => health, "Reports the service name and version";
//! }
//!
//! // emits, from that one entry (abbreviated):
//! pub fn register(router: ..) -> .. {
//!     router.route("/v1/services/health", axum::routing::on(MethodFilter::GET, health))
//! }
//! pub const ENDPOINTS: &[crate::server::EndpointMeta] = &[EndpointMeta {
//!     method: "GET",  // stringify! of the verb above, not a re-typed string
//!     path: "/v1/services/health",
//!     summary: "Reports the service name and version",
//! }];
//! ```
//!
//! A verb is written once: registration uses `axum::routing::MethodFilter::<VERB>` and the
//! metadata uses `stringify!` of that same identifier, so [`GENERATED_METADATA_FUNCTION`]
//! can only ever list what the router actually mounted. Nothing in `build_support/` reads a
//! route file's contents, so the listing is not a parse of the source.
//!
//! Handlers are plain axum handlers and may use the normal extractors
//! (`State`/`Path`/`Query`/`Json`). There is deliberately **no** runtime registry: a bad
//! route file is a native compile error with a normal rustc diagnostic pointing at the
//! route file (intentional, see `emit.rs`), never a runtime import failure.
//!
//! # What re-runs the scan (and what does not)
//!
//! Cargo only offers *recursive* `rerun-if-changed` directories and exact file watches, and
//! it ignores this scanner's prune rules when it expands a directory - so watching the scan
//! root would watch the build's own output (`target-dir = .rust/target`) and re-run the scan
//! forever. `discovery::watch_set` therefore watches the endpoint trees that produced
//! routes, each route file, and the ignore files above those trees, which keeps these
//! changes automatic:
//!
//! * add / change / remove a route file inside a discovered endpoint tree;
//! * add / remove a whole route **subdirectory** inside such a tree;
//! * edit an ignore file inside such a tree, or edit an ignore file that already existed
//!   above it.
//!
//! These need an explicit re-scan (documented in README, automated for the npm scripts by
//! `scripts/discover-services.mjs` writing the [`SNAPSHOT_PATH`] file that `build.rs`
//! watches, and available by hand through [`RESCAN_ENV`]):
//!
//! * a **brand-new `endpoint`/`endpoints` tree** outside an already watched one;
//! * an ignore file that is **newly created** above a watched tree (only existing paths can
//!   be watched, and a missing watch path is permanently dirty);
//! * changing a pruned directory name so that it becomes scannable.
//!
//! # What is *not* promised
//!
//! Ignore handling is the standard gitignore semantics of the `ignore` crate - not
//! bit-exact TypeScript-tooling equivalence. The scan root is the boundary: ignore files
//! above it are not consulted (`discovery.rs` sets `parents(false)`), and the user's
//! global gitignore is not consulted (`git_global(false)`) so the generated file does not
//! change between machines.

// This tree is compiled twice, into two different crates (build.rs and
// tests/discovery.rs). Each consumer uses a subset of the surface, so per-crate
// `dead_code` warnings are expected noise, not signal; the test crate is where the
// unused-by-build.rs helpers earn their keep.
#![allow(dead_code)]

pub mod discovery;
pub mod emit;
pub mod matching;
pub mod naming;
pub mod types;

// No `pub use` re-export block on purpose: this tree is compiled into a build script and a
// test crate (both effectively private), so an unused re-export is an `unused_imports`
// warning there. Consumers name the module they want:
// `build_support::discovery::scan_routes`, `build_support::types::ScanOptions`,
// `build_support::matching::is_route_file_name`, `build_support::naming::module_name_for`,
// `build_support::emit::generate_module`.

/// Compile-time override for the scan root. `build.rs` emits
/// `cargo::rerun-if-env-changed` for this key, so setting/unsetting it re-runs the scan
/// even when no file inside the previous root changed.
pub const ROUTE_ROOT_ENV: &str = "TYPECRATE_SERVICES_ROUTE_ROOT";

/// Manual rescan switch (T14/F1). `build.rs` emits
/// `cargo::rerun-if-env-changed` for this key, so **any change of its value** - including
/// setting it when it was unset, or unsetting it - re-runs the scan with no file edits and
/// no `cargo clean`.
///
/// Why a switch is needed at all: cargo can only watch a `rerun-if-changed` *path*
/// recursively, and the recursive form is what made the whole scan root (build artifacts
/// included) a dependency, so the watch set is deliberately narrow now (see
/// [`discovery`]). That narrow set cannot see a brand-new `endpoint`/`endpoints` tree
/// appear outside an already watched one. Setting this variable to anything new is the
/// documented way to force that discovery; `scripts/discover-services.mjs` automates the
/// same thing for the npm scripts through [`SNAPSHOT_PATH`].
pub const RESCAN_ENV: &str = "TYPECRATE_SERVICES_RESCAN";

/// Where the npm discovery pre-step records its snapshot: segments below the **repository
/// root** (the same root [`DEFAULT_ROOT_ANCESTOR_STEPS`] derives), i.e.
/// `<repo>/Temp/typecrate-services/route-discovery.json`.
///
/// `Temp/` is the repository's git-ignored scratch area (AGENTS.md), which is exactly right
/// for a rebuild cache: deleting it costs one re-scan, nothing else. It is also *below* a
/// pruned directory name, so the snapshot never becomes a discovered route or a watched
/// directory itself.
///
/// The file is a watch target, not an input: `build.rs` only stats it (and creates a seed
/// when it is missing), while `scripts/discover-services.mjs` rewrites it only when its
/// computed bytes change. Cargo has no shallow directory watch, so a *file* mtime is the
/// cheapest signal that can say "the tree shape changed".
pub const SNAPSHOT_PATH: [&str; 3] = ["Temp", "typecrate-services", "route-discovery.json"];

/// Bytes `build.rs` seeds a missing [`SNAPSHOT_PATH`] with. Cargo treats a *missing*
/// `rerun-if-changed` path as permanently dirty (probed in the T14 cargo-semantics probe),
/// so the file has to exist before it is watched; the seed content itself is never parsed.
pub const SNAPSHOT_SEED: &str = "{\"revision\":\"seed\",\"writtenBy\":\"build.rs\"}\n";

/// Default scan root = the repository root: this package lives at
/// `packages/typecrate/services`, three levels deep (R2). Expressed as an ancestor
/// *count* rather than a `../../..` string so the resolved root carries no `..`
/// components (Rust 2024 changed how `..` is handled at module load time, and the
/// generated `#[path]` values inherit whatever this produces).
pub const DEFAULT_ROOT_ANCESTOR_STEPS: usize = 3;

/// Name of the generated module inside `$OUT_DIR`.
pub const GENERATED_FILE_NAME: &str = "service_routes.rs";

/// Entry point emitted into the generated file.
pub const GENERATED_FUNCTION: &str = "register_routes";

/// Second crate-scope entry point emitted into the generated file (R3): the discovered
/// endpoint listing, in the same module order as [`GENERATED_FUNCTION`]. Each row comes
/// from a route module's [`ROUTE_ENDPOINTS_CONST`], i.e. from the same macro entries that
/// built its `register` fn, so the listing cannot disagree with the router. The type it
/// returns is `crate::server::EndpointMeta` (`src/server/metadata.rs`).
pub const GENERATED_METADATA_FUNCTION: &str = "route_metadata";

/// The `pub const` every discovered route module exposes, listing its own endpoints in the
/// order the file writes them. `emit.rs` only ever *names* it (`<module>::ENDPOINTS`);
/// `crate::service_routes!` ([`ROUTE_MACRO_PATH`]) is the only writer, which is what keeps
/// the const in step with the module's `register` fn.
pub const ROUTE_ENDPOINTS_CONST: &str = "ENDPOINTS";

/// How a route file spells the macro that declares its endpoints: [`ROUTE_ENDPOINTS_CONST`]
/// plus the contract-shaped `register` fn, from one entry list
/// (`src/server/metadata.rs`). Documentation and the contract string pinned by
/// `tests/discovery.rs` - the generator never emits an invocation, because route files are
/// hand-written and their contents are never parsed.
pub const ROUTE_MACRO_PATH: &str = "crate::service_routes";

/// The exact tail a route file name must end with: `<stem>-route.rs`, where `<stem>` is
/// **non-empty** and the match is case-sensitive (`health-route.rs`, `locations-route.rs`,
/// and the legacy `service-route.rs`, whose stem happens to be `service`).
///
/// Everything else inside an `endpoint`/`endpoints` tree is ignored, and that is the whole
/// point of the suffix form: the implementation files that live next to a route file
/// (`<stem>-core.rs`, README "How an example endpoint is laid out") are compiled because the
/// route file pulls them in with its own `#[path]`, never because the scanner found them. The
/// old `service-route*` **prefix** spellings (`service-route-echo.rs`) do not end in
/// `-route.rs`, so renaming a route file away from the suffix silently drops its routes -
/// pinned by `tests/discovery.rs` because nothing else reports it.
pub const ROUTE_FILE_SUFFIX: &str = "-route.rs";

/// Extension every route file carries (lowercase; `health-route.RS` does not match).
///
/// Kept separate from [`ROUTE_FILE_SUFFIX`] on purpose: matching requires the whole
/// `-route.rs` tail, while [`naming::module_name_for`] strips only this extension, which is
/// what makes a generated identifier keep the `-route` half (`health-route.rs` ->
/// `..._health_route`) and stay readable next to its file.
pub const ROUTE_FILE_EXTENSION: &str = ".rs";

/// A file name whose **stem** contains one of these fragments is a test source, never a
/// route (`service.test.-route.rs`, `pets.spec.-route.rs`). The dots are part of the marker,
/// so `pets.test-route.rs` is still a route: only a dotted `.test.`/`.spec.` segment counts.
pub const TEST_NAME_MARKERS: [&str; 2] = [".test.", ".spec."];

/// Directory names that open a route tree. Both spellings are aliases on purpose: the
/// repository already contains `src/server/endpoint/...` and `src/server/endpoints/...`
/// trees. Exact, case-sensitive match (`Endpoints/` is not a route tree).
pub const ENDPOINT_DIR_NAMES: [&str; 2] = ["endpoint", "endpoints"];

/// Ignore files the `ignore` crate reads per directory. `build.rs` watches the ones that
/// exist **strictly above** a watched endpoint tree (`discovery::watch_set`), because an
/// ignore file outside a watched directory changes discovery without touching any watched
/// path; the ones inside an endpoint tree are covered by that tree's recursive watch.
/// `.git/info/exclude` is joined onto each candidate directory (it stays a valid relative
/// path when the directory has no `.git`).
pub const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".git/info/exclude"];

/// Directory names that are never descended into, **even when no ignore rule covers
/// them** (generated output, dependency and scratch trees). Matched case-insensitively
/// against the single directory name. The scan root itself is exempt: `discovery.rs`
/// checks `matching::is_scan_root` before `matching::is_pruned_dir_name`.
///
/// Deliberately *not* listed: `lib`, `app`, `out`, `build` - those names are too generic
/// to prune by name alone, and the repository root `.gitignore` already ignores them at
/// every depth, so gitignore-awareness covers them.
///
/// Escape hatch: point `TYPECRATE_SERVICES_ROUTE_ROOT` straight at a route tree to scan
/// inside a pruned name.
pub const PRUNED_DIR_NAMES: &[&str] = &[
    ".cache",
    ".cargo",
    ".git",
    ".hg",
    ".next",
    ".rust",
    ".svn",
    ".turbo",
    ".venv",
    "__pycache__",
    "coverage",
    "dist",
    "node_modules",
    "pkg",
    "storybook-static",
    "target",
    "temp",
    "tmp",
    "venv",
];
