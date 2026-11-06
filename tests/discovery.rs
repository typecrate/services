//! Tests for the build-time service-route scanner (`build_support/`).
//!
//! The whole `build_support` tree is compiled into this test crate by path: a build script
//! has no rlib an integration test can link, so `#[path]` is what lets these assertions pin
//! the real rules instead of only observing a generated file.
//!
//! Fixtures are real directory trees under the repository-root `Temp/` (AGENTS.md: never OS
//! temp), one directory per test named `<pid>-<test name>`, removed in `Drop`. That keeps
//! concurrent test threads and concurrent test processes apart.
//!
//! Not covered here (needs a cargo build, deferred to the integration pass): that the
//! generated module compiles inside the crate root, that `crate::route_metadata()` really
//! returns the example endpoints, and that none of this reaches the wasm32 artifact - all
//! three are compile-time properties of `src/` (T10/T18), asserted by
//! `npm run wasm:check` / `lint:strict` and by the HTTP tests. The same is true of the T14
//! rules: this file pins the *watch set* and the generated `#[allow(non_snake_case)]`
//! attribute exactly, while "cargo re-runs the scan for those paths" and "those identifiers
//! pass `-D warnings`" are properties of cargo and rustc (verified with an isolated `Temp/`
//! scan root plus `cargo clippy -- -D warnings`, and a cargo-semantics probe crate for the
//! watch matrix).
//!
//! What IS pinned for R3 (T17): the generated `route_metadata` item byte for byte, in every
//! tree shape (multiple routes, zero routes, identifiers that need the snake-case allow), and
//! that it names the SAME module identifiers in the SAME order as `register_routes`. The
//! method/path/summary *values* are deliberately out of reach here: the scanner never opens
//! a route file, so those values come from each file's `crate::service_routes!` invocation
//! (`src/server/metadata.rs`), where one token feeds both the mount and the listing -
//! a compile-time property checked by that macro's own unit tests, not by a filesystem walk.

// `build_support` pulls in the `ignore` crate for its walk; that dependency is host-only
// (see `Cargo.toml`), so this whole file is compiled out for the wasm target.
#![cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]

#[path = "../build_support/mod.rs"]
mod build_support;

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use build_support::DEFAULT_ROOT_ANCESTOR_STEPS;
use build_support::ENDPOINT_DIR_NAMES;
use build_support::GENERATED_FILE_NAME;
use build_support::GENERATED_FUNCTION;
use build_support::GENERATED_METADATA_FUNCTION;
use build_support::RESCAN_ENV;
use build_support::ROUTE_ENDPOINTS_CONST;
use build_support::ROUTE_FILE_EXTENSION;
use build_support::ROUTE_FILE_SUFFIX;
use build_support::ROUTE_MACRO_PATH;
use build_support::ROUTE_ROOT_ENV;
use build_support::SNAPSHOT_PATH;
use build_support::SNAPSHOT_SEED;
use build_support::TEST_NAME_MARKERS;
use build_support::discovery::ensure_snapshot_file;
use build_support::discovery::resolve_route_root;
use build_support::discovery::scan_routes;
use build_support::discovery::snapshot_file_path;
use build_support::emit::generate_module;
use build_support::emit::write_generated_if_changed;
use build_support::matching::ancestor_dirs;
use build_support::matching::endpoint_tree_root;
use build_support::matching::is_endpoint_dir_name;
use build_support::matching::is_pruned_dir_name;
use build_support::matching::is_route_file_name;
use build_support::matching::relative_path;
use build_support::naming::assign_module_names;
use build_support::naming::module_name_for;
use build_support::naming::needs_snake_case_allow;
use build_support::types::ScanOptions;
use build_support::types::ScanReport;

/// Body of every fixture route file: contents are irrelevant to discovery (the scanner
/// never reads route files), but a real file has to exist on disk. This is also why the R3
/// listing cannot come from a scan: with contents unread, the only trustworthy source of a
/// route file's methods and paths is the file's own `crate::service_routes!` expansion
/// (`src/server/metadata.rs`), which the generated `route_metadata` merely concatenates.
const FIXTURE_ROUTE: &str = "// fixture route\n";

/// This package's directory, as cargo reports it (absolute).
const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Repository root: `DEFAULT_ROOT_ANCESTOR_STEPS` levels above this package - the same rule
/// `build.rs` uses, re-derived here so the test asserts the rule rather than trusting it.
fn repo_root() -> PathBuf {
    Path::new(MANIFEST_DIR)
        .ancestors()
        .nth(DEFAULT_ROOT_ANCESTOR_STEPS)
        .expect("the package dir has an ancestor chain this deep")
        .to_path_buf()
}

/// Scratch fixture for one test.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    /// Creates an empty fixture tree, clearing a stale directory left by an aborted run.
    fn new(test_name: &str) -> Self {
        let root = repo_root()
            .join("Temp")
            .join("typecrate-services-discovery")
            .join(format!("{}-{test_name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root)
            .unwrap_or_else(|err| panic!("could not create fixture `{}`: {err}", root.display()));
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    /// Writes `contents` at `relative` (forward slashes), creating parent directories.
    fn write(&self, relative: &str, contents: &str) -> &Self {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .unwrap_or_else(|err| panic!("could not create `{}`: {err}", parent.display()));
        }
        fs::write(&path, contents)
            .unwrap_or_else(|err| panic!("could not write `{}`: {err}", path.display()));
        self
    }

    /// Runs the real scanner, failing loudly on a traversal error: a fixture must be clean.
    fn scan(&self) -> ScanReport {
        let report = scan_routes(&ScanOptions {
            root: self.root.clone(),
        })
        .unwrap_or_else(|err| panic!("scan of `{}` must succeed: {err}", self.root.display()));
        assert!(
            report.errors.is_empty(),
            "fixture scan reported traversal errors: {:?}",
            report.errors
        );
        assert_eq!(report.root, self.root, "the report must echo the scan root");
        report
    }

    /// The route set as complete `relative path|module identifier` values.
    fn routes(&self) -> Vec<String> {
        route_summaries(&self.scan())
    }

    /// The cargo watch set, rendered scan-root-relative so the exact expectations below
    /// read the same on Windows and POSIX (they are absolute paths on disk).
    fn watch(&self) -> Vec<String> {
        self.scan()
            .watch_paths
            .iter()
            .map(|path| relative_path(self.root(), path))
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Best effort: the tree is disposable scratch state under `Temp/`.
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn route_summaries(report: &ScanReport) -> Vec<String> {
    report
        .routes
        .iter()
        .map(|route| format!("{}|{}", route.relative_path, route.module_name))
        .collect()
}

/// Start of the generated registration item (its doc comment), used to slice the emitted
/// text without depending on the human-readable header above it.
const REGISTER_MARKER: &str = "/// Registers every discovered service route";

/// Start of the generated endpoint-listing item (R3), the second slice point.
const METADATA_MARKER: &str = "/// Metadata for every discovered service route";

/// The `register_routes` item: from its doc comment up to the `route_metadata` item, i.e.
/// exactly what T10's server module calls, plus the blank line `emit.rs` writes between the
/// two items (so a byte expectation of this section ends at `}\n\n`).
fn generated_body(generated: &str) -> &str {
    let start = generated.find(REGISTER_MARKER).unwrap_or_else(|| {
        panic!("generated module must define the registration function:\n{generated}")
    });
    let end = generated.find(METADATA_MARKER).unwrap_or_else(|| {
        panic!("generated module must define the metadata function:\n{generated}")
    });
    &generated[start..end]
}

/// The `route_metadata` item (R3): from its doc comment to the end of the file - it is the
/// last thing `emit.rs` renders, which is what makes the exact-byte expectations below
/// self-contained (no absolute path in this section, ever: it names module identifiers only).
fn generated_metadata(generated: &str) -> &str {
    let start = generated.find(METADATA_MARKER).unwrap_or_else(|| {
        panic!("generated module must define the metadata function:\n{generated}")
    });
    &generated[start..]
}

/// The module identifiers a generated section refers to, in the order it refers to them.
///
/// `needle` is the suffix that follows an identifier (`"::register(router)"` in the chain,
/// `"::ENDPOINTS"` in the listing). Scanning backwards to the first non-identifier character
/// keeps this a *reader* of the emitted text, not a reimplementation of the generator: it
/// only answers "which identifiers appear, in which order".
fn referenced_modules<'a>(section: &'a str, needle: &str) -> Vec<&'a str> {
    let mut modules: Vec<&'a str> = Vec::new();
    let mut rest = section;
    while let Some(at) = rest.find(needle) {
        let head = &rest[..at];
        // Byte index just past the last character that cannot be part of an identifier.
        let start = head
            .char_indices()
            .rfind(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
            .map_or(0, |(index, c)| index + c.len_utf8());
        modules.push(&head[start..]);
        rest = &rest[at + needle.len()..];
    }
    modules
}

/// `true` when `name` is a usable Rust identifier under this generator's policy
/// (ASCII-only, `_` or letter first, never a bare keyword).
fn assert_valid_identifier(name: &str) {
    let mut chars = name.chars();
    let first = chars
        .next()
        .unwrap_or_else(|| panic!("generated identifier must not be empty"));
    assert!(
        first == '_' || first.is_ascii_alphabetic(),
        "`{name}` must start with `_` or an ASCII letter"
    );
    assert!(
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "`{name}` must be ASCII alphanumeric or `_`"
    );
}

// ---------------------------------------------------------------------------
// The contract strings T10 (server module + HTTP tests) consumes.
// ---------------------------------------------------------------------------

#[test]
fn the_generated_file_contract_names_are_pinned() {
    assert_eq!(ROUTE_ROOT_ENV, "TYPECRATE_SERVICES_ROUTE_ROOT");
    // The explicit re-scan switch `scripts/discover-services.mjs` and README both name.
    assert_eq!(RESCAN_ENV, "TYPECRATE_SERVICES_RESCAN");
    assert_eq!(GENERATED_FILE_NAME, "service_routes.rs");
    assert_eq!(GENERATED_FUNCTION, "register_routes");
    // R3 (T17): the listing half of the generated file, plus the two names a route file's
    // declaration has to line up with. `src/server/mod.rs:42-43` re-exports the DTO and its
    // `GET /v1/services/routes` handler calls `crate::route_metadata()`, so renaming any of
    // these three strings means changing the generator, the macro and this pin together.
    assert_eq!(GENERATED_METADATA_FUNCTION, "route_metadata");
    assert_eq!(ROUTE_ENDPOINTS_CONST, "ENDPOINTS");
    assert_eq!(ROUTE_MACRO_PATH, "crate::service_routes");
    // `#[macro_export]` puts the macro at the crate root, which is why the contract spells it
    // as a `crate::`-rooted path: a route file needs no helper `use` at all.
    assert!(ROUTE_MACRO_PATH.starts_with("crate::"));
    assert_eq!(ENDPOINT_DIR_NAMES, ["endpoint", "endpoints"]);
    assert_eq!(TEST_NAME_MARKERS, [".test.", ".spec."]);
    // The naming contract (R1): a route file is `<non-empty stem>-route.rs`. `build.rs`
    // spells this tail in its "no route file found" warning, `scripts/discover-services.mjs`
    // mirrors it in `isRouteFileName`, and README documents it, so both constants are pinned
    // here rather than restated.
    assert_eq!(ROUTE_FILE_SUFFIX, "-route.rs");
    assert_eq!(ROUTE_FILE_EXTENSION, ".rs");
    // The suffix carries the extension, which is why matching needs only the suffix while
    // `naming::module_name_for` strips the extension alone (identifiers end in `_route`).
    assert!(ROUTE_FILE_SUFFIX.ends_with(ROUTE_FILE_EXTENSION));
    assert!(
        ROUTE_FILE_SUFFIX.starts_with('-'),
        "the stem half of the name is what says which resource a route file mounts"
    );
    // packages/typecrate/services -> repository root.
    assert_eq!(DEFAULT_ROOT_ANCESTOR_STEPS, 3);
    // The snapshot contract with the npm pre-step: repo-scratch path + seed bytes. These
    // are the strings `scripts/discover-services.mjs` mirrors, so a rename on one side
    // fails here instead of silently disabling new-endpoint-folder pickup.
    assert_eq!(
        SNAPSHOT_PATH,
        ["Temp", "typecrate-services", "route-discovery.json"]
    );
    assert_eq!(
        SNAPSHOT_SEED,
        "{\"revision\":\"seed\",\"writtenBy\":\"build.rs\"}\n"
    );
}

// ---------------------------------------------------------------------------
// R2: scan root (default, override, misconfiguration).
// ---------------------------------------------------------------------------

#[test]
fn the_default_root_is_the_repository_root_and_the_override_wins() {
    let manifest = Path::new(MANIFEST_DIR);
    let fixture = Fixture::new("the_default_root_is_the_repository_root_and_the_override_wins");
    // Default (R2): the repository root, resolved without touching the environment.
    assert_eq!(
        resolve_route_root(manifest, None).expect("default root"),
        repo_root()
    );
    // Relative overrides resolve against the package dir, `.`/`..` folded lexically.
    assert_eq!(
        resolve_route_root(manifest, Some(".")).expect("`.`"),
        manifest
    );
    assert_eq!(
        resolve_route_root(manifest, Some("../..")).expect("`..` levels"),
        manifest.ancestors().nth(2).expect("two levels up exists")
    );
    assert_eq!(
        resolve_route_root(manifest, Some("../../..")).expect("three levels up"),
        repo_root()
    );
    assert_eq!(
        resolve_route_root(manifest, Some("../../../packages/typecrate/services"))
            .expect("folded round trip"),
        manifest
    );
    // A resolved root never carries `..` (the generated `#[path]` values inherit it).
    let folded = resolve_route_root(manifest, Some("./../../../packages/./typecrate/services"))
        .expect("messy input");
    assert_eq!(folded, manifest);
    assert!(
        !folded.to_string_lossy().contains(".."),
        "`{}` must be `..`-free",
        folded.display()
    );
    // An absolute override is taken as given.
    assert_eq!(
        resolve_route_root(manifest, Some(&fixture.root().display().to_string()))
            .expect("absolute override"),
        fixture.root()
    );
    // `set X=` / `export X=` leave an empty value: that has to mean "use the default", not
    // "scan the current directory".
    assert_eq!(
        resolve_route_root(manifest, Some("")).expect("empty override"),
        repo_root()
    );
    assert_eq!(
        resolve_route_root(manifest, Some("   ")).expect("blank override"),
        repo_root()
    );
    // A `..` past the top of the filesystem must CLAMP, not pop the root away: with the
    // naive `PathBuf::pop` fold, `D:\..\..` becomes an empty/relative buffer (and then
    // either "not a directory" or, worse, a scan of the process cwd). Same rule as
    // `src/server/mod.rs::normalize_lexically`.
    let clamped = resolve_route_root(
        manifest,
        Some("../../../../../../../../../../../../../../.."),
    )
    .expect("an over-deep `..` chain resolves to the filesystem root");
    assert!(
        clamped.parent().is_none(),
        "`{}` is a filesystem root and has no parent",
        clamped.display()
    );
    assert!(clamped.is_absolute());
    assert!(
        !clamped.to_string_lossy().contains(".."),
        "`{}` must be `..`-free",
        clamped.display()
    );
}

#[test]
fn a_scan_root_that_is_not_a_directory_fails_with_the_env_var_name() {
    let manifest = Path::new(MANIFEST_DIR);
    let fixture = Fixture::new("a_scan_root_that_is_not_a_directory_fails_with_the_env_var_name");
    let missing = fixture.root().join("nope");
    let error = resolve_route_root(manifest, Some(&missing.display().to_string()))
        .expect_err("a missing root must fail");
    // The exact message `build.rs` prints (it adds only its own crate prefix), because an
    // env-var misconfiguration is the failure a developer actually hits.
    assert_eq!(
        error,
        format!(
            "`{ROUTE_ROOT_ENV}` resolved to `{missing}`, which is not a directory. \
             Create it or point `{ROUTE_ROOT_ENV}` at the folder that holds your `endpoint`/`endpoints` trees.",
            missing = missing.display()
        )
    );
    // The other misconfiguration mode: the path exists but is a file.
    fixture.write("plain.txt", "not a directory\n");
    let as_file = fixture.root().join("plain.txt");
    let error = resolve_route_root(manifest, Some(&as_file.display().to_string()))
        .expect_err("a file is not a scan root");
    assert!(
        error.contains("is not a directory") && error.contains(ROUTE_ROOT_ENV),
        "the message must name the env var and the problem: {error}"
    );
    // `scan_routes` guards the same precondition so a caller that skipped resolution still
    // gets a structured error instead of an empty report.
    let scan_error = scan_routes(&ScanOptions {
        root: as_file.clone(),
    })
    .expect_err("a file cannot be scanned");
    assert_eq!(
        scan_error.to_string(),
        format!("scan root `{}` is not a directory", as_file.display())
    );
}

// ---------------------------------------------------------------------------
// R2/R3: what is discovered.
// ---------------------------------------------------------------------------

#[test]
fn endpoint_and_endpoints_trees_are_both_scanned() {
    let fixture = Fixture::new("endpoint_and_endpoints_trees_are_both_scanned");
    fixture
        // Both aliases are route trees.
        .write("endpoint/health/health-route.rs", FIXTURE_ROUTE)
        .write("endpoints/echo/echo-route.rs", FIXTURE_ROUTE)
        // A route file name with no endpoint-tree ancestor is not an endpoint.
        .write("services/health/health-route.rs", FIXTURE_ROUTE);

    assert_eq!(
        fixture.routes(),
        [
            // `e` (0x65) sorts before `s`, so the singular alias comes first.
            "endpoint/health/health-route.rs|endpoint_health_health_route",
            "endpoints/echo/echo-route.rs|endpoints_echo_echo_route",
        ]
    );

    // The report also carries absolute paths, the root, and identifiers that are real
    // Rust identifiers (they are emitted as `mod` names).
    let report = fixture.scan();
    assert_eq!(
        report.routes[0].path,
        fixture
            .root()
            .join("endpoint")
            .join("health")
            .join("health-route.rs")
    );
    assert_eq!(report.routes[0].root, fixture.root());
    assert!(report.routes[1].path.is_absolute());
    for route in &report.routes {
        assert_valid_identifier(&route.module_name);
    }
}

#[test]
fn suffix_variants_match_and_test_or_spec_variants_do_not() {
    let fixture = Fixture::new("suffix_variants_match_and_test_or_spec_variants_do_not");
    for name in [
        // Accepted: the stem is free-form, only the `-route.rs` tail is fixed.
        "health-route.rs",
        "health-echo-id-route.rs",
        "pets.test-route.rs",
        // Accepted, and load-bearing: the legacy bare spelling still matches the new rule
        // because its stem is `service`. No special case exists for it.
        "service-route.rs",
        // Ignored, one cause each.
        "mod.rs",
        "index.rs",
        "route.rs",
        "-route.rs",
        "pets-routes.rs",
        "service-routes.rs",
        "health-core.rs",
        "health-route.ts",
        // The migration hazard, on disk: the old `service-route*` prefix spelling is now an
        // ordinary module file, its routes simply leave the build (see also
        // `the_old_prefixed_spellings_are_the_migration_hazard`).
        "service-route-echo.rs",
        // A marker only counts as a dotted `.test.`/`.spec.` segment, hence the `-`-joined
        // `pets.test-route.rs` above IS a route and these two are not.
        "health.test.-route.rs",
        "pets.spec.-route.rs",
        // Written last on purpose: on a case-insensitive filesystem this is the same entry as
        // a lowercase `pets-route.rs` would be, and there is none here, so the on-disk
        // spelling stays mixed-case and the case-sensitive rule is what rejects it.
        "Pets-Route.rs",
    ] {
        fixture.write(&format!("endpoints/health/{name}"), FIXTURE_ROUTE);
    }

    assert_eq!(
        fixture.routes(),
        [
            // `h` (0x68) < `p` (0x70) < `s` (0x73); inside `health-`, `e` < `r`.
            "endpoints/health/health-echo-id-route.rs|endpoints_health_health_echo_id_route",
            "endpoints/health/health-route.rs|endpoints_health_health_route",
            "endpoints/health/pets.test-route.rs|endpoints_health_pets_test_route",
            "endpoints/health/service-route.rs|endpoints_health_service_route",
        ]
    );
}

#[test]
fn the_old_prefixed_spellings_are_the_migration_hazard() {
    // R1, the case a route author actually trips over: the tree keeps the pre-rename file
    // names, and nothing errors. Echo/pets/locations stay on disk as plain modules and drop
    // out of both the router and `GET /v1/services/routes`; only a `*-route.rs` file is
    // discovered. Every ignored file below matches on *one* axis only (a route-shaped name,
    // or an endpoint-tree location), so each absence has exactly one cause.
    let fixture = Fixture::new("the_old_prefixed_spellings_are_the_migration_hazard");
    for relative in [
        // The three old multi-suffix spellings, unchanged from the examples tree.
        "endpoints/echo/service-route-echo.rs",
        "endpoints/pets/service-route-pets.rs",
        "endpoints/locations/service-route-locations.rs",
        // Implementation files that live beside a route file (README "two-file template"):
        // compiled through the route file's `#[path]`, never through discovery.
        "endpoints/pets/pets-core.rs",
        "endpoints/pets/mod.rs",
        // Near-misses that must not be accepted by accident.
        "endpoints/pets/pets-routes.rs",
        "endpoints/case/Pets-Route.rs",
        "endpoints/empty/-route.rs",
        "endpoints/spec/pets.spec.-route.rs",
        // The two that must survive: a fresh `<resource>-route.rs`, and the legacy bare
        // `service-route.rs` whose stem (`service`) still satisfies the pattern.
        "endpoints/health/health-route.rs",
        "endpoints/extra/extra-pets-route.rs",
        "endpoints/legacy/service-route.rs",
    ] {
        fixture.write(relative, FIXTURE_ROUTE);
    }

    assert_eq!(
        fixture.routes(),
        [
            // `e` < `h` < `l` byte-wise inside `endpoints/`.
            "endpoints/extra/extra-pets-route.rs|endpoints_extra_extra_pets_route",
            "endpoints/health/health-route.rs|endpoints_health_health_route",
            "endpoints/legacy/service-route.rs|endpoints_legacy_service_route",
        ]
    );
    // Same shape from the cargo side: the ignored files are not route inputs, so they add no
    // `rerun-if-changed` path even though they sit in the watched tree (editing
    // `service-route-echo.rs` no longer rebuilds the route table).
    assert_eq!(
        fixture.watch(),
        [
            "endpoints",
            "endpoints/extra/extra-pets-route.rs",
            "endpoints/health/health-route.rs",
            "endpoints/legacy/service-route.rs",
        ]
    );
    // And the generated table is exactly those three modules - the pre-rename routes are gone.
    let generated = generate_module(&fixture.scan());
    assert!(generated.contains("// Routes found: 3"));
    for dropped in [
        "endpoints_echo_service_route_echo",
        "endpoints_pets_service_route_pets",
        "endpoints_locations_service_route_locations",
    ] {
        assert!(
            !generated.contains(dropped),
            "`{dropped}` must not be compiled in:\n{generated}"
        );
    }
}

#[test]
fn route_files_outside_an_endpoint_tree_are_never_registered() {
    let fixture = Fixture::new("route_files_outside_an_endpoint_tree_are_never_registered");
    for relative in [
        "src/health-route.rs",
        "endpoints-old/health-route.rs",
        "endpointz/health-route.rs",
        "nested/deeper/health-route.rs",
        // The one genuine hit: a route file directly inside the alias directory (no extra
        // nesting) still counts.
        "endpoints/root-route.rs",
        "endpoints/health/health-route.rs",
    ] {
        fixture.write(relative, FIXTURE_ROUTE);
    }

    assert_eq!(
        fixture.routes(),
        [
            "endpoints/health/health-route.rs|endpoints_health_health_route",
            "endpoints/root-route.rs|endpoints_root_route",
        ]
    );
}

#[test]
fn nested_ignore_rules_are_honoured_and_bang_rules_re_include() {
    let fixture = Fixture::new("nested_ignore_rules_are_honoured_and_bang_rules_re_include");
    fixture
        // Root rule: every `draft-*-route.rs` is ignored, then one is whitelisted again. An
        // unanchored pattern applies at every depth, so it also covers `echo/`
        // below; the `draft` marker is deliberate so the nested rules further down cannot be
        // explained by this rule (each expectation below has exactly one cause). Every file
        // here is `*-route.rs` shaped, so nothing is hidden by the name rule.
        .write(".gitignore", "draft-*-route.rs\n!draft-keep-route.rs\n")
        .write("endpoints/echo/draft-keep-route.rs", FIXTURE_ROUTE)
        .write("endpoints/echo/draft-park-route.rs", FIXTURE_ROUTE)
        // A deeper `.gitignore` outranks the shallower one inside its own subtree, and here
        // it is the ONLY rule that touches the file: `internal-route.rs` is not a
        // `draft`, so its absence can only come from `health/.gitignore`.
        .write("endpoints/health/.gitignore", "internal-route.rs\n")
        .write("endpoints/health/health-route.rs", FIXTURE_ROUTE)
        .write("endpoints/health/internal-route.rs", FIXTURE_ROUTE)
        // A directory ignored wholesale is not descended into (git semantics: files inside
        // an excluded directory cannot be re-included).
        .write("endpoints/skip/.gitignore", "generated/\n")
        .write("endpoints/skip/generated/skip-route.rs", FIXTURE_ROUTE)
        // `.ignore` is honoured with the same rule language, and again it is the only
        // possible cause here (a `tool` is not a `draft`); `list` is the sibling control
        // proving the rule hid one file rather than the whole tree.
        .write("endpoints/tools/.ignore", "tool-route.rs\n")
        .write("endpoints/tools/tool-route.rs", FIXTURE_ROUTE)
        .write("endpoints/tools/list-route.rs", FIXTURE_ROUTE);

    assert_eq!(
        fixture.routes(),
        [
            "endpoints/echo/draft-keep-route.rs|endpoints_echo_draft_keep_route",
            "endpoints/health/health-route.rs|endpoints_health_health_route",
            "endpoints/tools/list-route.rs|endpoints_tools_list_route",
        ]
    );
}

#[test]
fn generated_and_scratch_directories_are_pruned_even_without_ignore_rules() {
    let fixture =
        Fixture::new("generated_and_scratch_directories_are_pruned_even_without_ignore_rules");
    // Nothing in this fixture is gitignored except where a rule is written below, so the
    // only thing that can hide these trees is `PRUNED_DIR_NAMES`. `Temp`/`TEMP` and
    // `.rust`/`.turbo` also prove the name match is case-insensitive and that hidden
    // artifact trees are skipped.
    for dir in [
        "node_modules",
        "Temp",
        "TEMP",
        "tmp",
        "pkg",
        "target",
        "dist",
        "coverage",
        ".rust",
        ".turbo",
        ".git",
    ] {
        fixture.write(&format!("{dir}/endpoints/health-route.rs"), FIXTURE_ROUTE);
    }
    // The control: an ordinary directory name is traversed.
    fixture.write("endpoints/health/health-route.rs", FIXTURE_ROUTE);

    assert_eq!(
        fixture.routes(),
        ["endpoints/health/health-route.rs|endpoints_health_health_route"]
    );
    // The watch set shows the same thing from the tracking side: a pruned directory is
    // never entered, so neither its route file nor its `endpoints` tree contributes a
    // `cargo::rerun-if-changed` path. Exact list under the T14 policy: the outermost
    // endpoint directory of the one discovered route, plus the route file itself. The
    // scan root is absent on purpose - cargo expands a directory watch recursively and the
    // build's own artifacts live under the root, which is the loop this prevents.
    assert_eq!(
        fixture.watch(),
        ["endpoints", "endpoints/health/health-route.rs"]
    );
}

#[test]
fn hidden_trees_are_not_route_sources() {
    let fixture = Fixture::new("hidden_trees_are_not_route_sources");
    fixture
        .write(".private/endpoints/health-route.rs", FIXTURE_ROUTE)
        .write("endpoints/.generated/health-route.rs", FIXTURE_ROUTE)
        .write("endpoints/health/health-route.rs", FIXTURE_ROUTE);

    assert_eq!(
        fixture.routes(),
        ["endpoints/health/health-route.rs|endpoints_health_health_route"]
    );
}

#[test]
fn symlinked_directories_and_files_are_not_followed() {
    let fixture = Fixture::new("symlinked_directories_and_files_are_not_followed");
    fixture.write("real/endpoints/health-route.rs", FIXTURE_ROUTE);
    let real_dir = fixture.root().join("real").join("endpoints");
    let real_file = real_dir.join("health-route.rs");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&real_dir, fixture.root().join("linked"))
            .expect("fixture directory symlink");
        std::os::unix::fs::symlink(&real_file, real_dir.join("mirror-route.rs"))
            .expect("fixture file symlink");
    }
    #[cfg(windows)]
    {
        // A Windows directory symlink needs SeCreateSymbolicLink (developer mode or admin).
        // If it cannot be created the scan simply has no symlinks, and the expectation
        // below is the same set either way - only the strength of the evidence differs.
        std::os::windows::fs::symlink_dir(&real_dir, fixture.root().join("linked")).ok();
        std::os::windows::fs::symlink_file(&real_file, real_dir.join("mirror-route.rs")).ok();
    }

    assert_eq!(
        fixture.routes(),
        ["real/endpoints/health-route.rs|real_endpoints_health_route"]
    );
}

// ---------------------------------------------------------------------------
// Determinism: order, identifiers, generated bytes.
// ---------------------------------------------------------------------------

#[test]
fn discovery_order_and_generated_bytes_are_deterministic() {
    let fixture = Fixture::new("discovery_order_and_generated_bytes_are_deterministic");
    fixture
        .write("endpoints/health/health-route.rs", FIXTURE_ROUTE)
        .write("endpoints/echo/echo-route.rs", FIXTURE_ROUTE)
        // Written last, sorted last (`metrics` > `health` > `echo`): creation order cannot
        // leak into output.
        .write("endpoints/metrics/metrics-route.rs", FIXTURE_ROUTE);

    let report = fixture.scan();
    assert_eq!(
        route_summaries(&report),
        [
            "endpoints/echo/echo-route.rs|endpoints_echo_echo_route",
            "endpoints/health/health-route.rs|endpoints_health_health_route",
            "endpoints/metrics/metrics-route.rs|endpoints_metrics_metrics_route",
        ]
    );
    // Repeated scans of an unchanged tree agree on every field, not just on the text.
    assert_eq!(report, fixture.scan());
    let generated = generate_module(&report);
    assert_eq!(generated, generate_module(&fixture.scan()));

    let echo = fixture
        .root()
        .join("endpoints")
        .join("echo")
        .join("echo-route.rs");
    let health = fixture
        .root()
        .join("endpoints")
        .join("health")
        .join("health-route.rs");
    let metrics = fixture
        .root()
        .join("endpoints")
        .join("metrics")
        .join("metrics-route.rs");
    // The emitted code, byte for byte: absolute `#[path]` module per route, then the two
    // crate-scope functions T10/T18 call (`register_routes`, then `route_metadata`).
    let declarations = generated
        .find("#[path = ")
        .map(|index| &generated[index..generated.find(REGISTER_MARKER).expect("registration doc")])
        .expect("a report with routes must declare modules");
    assert_eq!(
        declarations,
        format!(
            "#[path = \"{echo}\"]\nmod endpoints_echo_echo_route;\n\n\
             #[path = \"{health}\"]\nmod endpoints_health_health_route;\n\n\
             #[path = \"{metrics}\"]\nmod endpoints_metrics_metrics_route;\n\n",
            echo = echo.display().to_string().escape_debug(),
            health = health.display().to_string().escape_debug(),
            metrics = metrics.display().to_string().escape_debug(),
        )
    );
    assert_eq!(
        generated_body(&generated),
        "/// Registers every discovered service route onto `router`, in the order listed above.\n\
         pub fn register_routes(\n\
         \x20   router: axum::Router<crate::server::AppState>,\n\
         ) -> axum::Router<crate::server::AppState> {\n\
         \x20   let router = endpoints_echo_echo_route::register(router);\n\
         \x20   let router = endpoints_health_health_route::register(router);\n\
         \x20   endpoints_metrics_metrics_route::register(router)\n\
         }\n\n"
    );
    // R3: the companion listing, byte for byte, including the separator blank line above it
    // (part of `generated_body`) and the fact that it references the three modules in the
    // same order the chain does.
    assert_eq!(
        generated_metadata(&generated),
        "/// Metadata for every discovered service route, in the order listed above.\n\
         ///\n\
         /// Built-ins (`src/server`) are deliberately absent: this is the *discovered*\n\
         /// endpoint listing, and every row comes from the entries that module's\n\
         /// `crate::service_routes!` invocation already mounted, so it cannot disagree\n\
         /// with `register_routes` above.\n\
         pub fn route_metadata() -> Vec<crate::server::EndpointMeta> {\n\
         \x20   // `ENDPOINTS` of every discovered module, in registration order.\n\
         \x20   const DISCOVERED: &[&[crate::server::EndpointMeta]] = &[\n\
         \x20       endpoints_echo_echo_route::ENDPOINTS,\n\
         \x20       endpoints_health_health_route::ENDPOINTS,\n\
         \x20       endpoints_metrics_metrics_route::ENDPOINTS,\n\
         \x20   ];\n\
         \x20   DISCOVERED.concat()\n\
         }\n"
    );
    // The header states where the routes came from and how many there are.
    assert!(generated.contains(&format!("// Scan root: {}", fixture.root().display())));
    assert!(generated.contains("// Routes found: 3"));
    // And it names the include site the host actually uses (`src/lib.rs`, crate scope), so
    // the generated file's own instructions can never point somewhere other than the place
    // `crate::register_routes` is called from.
    assert!(generated.contains(
        "// Build-time service-route registration module. Include it at CRATE SCOPE, exactly"
    ));
    assert!(generated.contains("// as `src/lib.rs` does - the signatures below already name"));
    // The route-file contract the entries come from, stated in the file that consumes them.
    assert!(
        generated
            .contains("// Each route file declares its endpoints with `crate::service_routes!`")
    );
    assert!(generated.contains(
        "// `pub const ENDPOINTS` from the same entries. `route_metadata` below concatenates"
    ));
    assert!(generated.contains("// Discovered route file -> generated module identifier:"));
    // The path -> identifier map, one line per discovered file: this is what a reader of the
    // generated file uses to find the module behind a `*-route.rs`, so the `-route` half of
    // every identifier must line up with the `-route` half of every name on disk.
    for mapped in [
        "//   endpoints/echo/echo-route.rs -> mod endpoints_echo_echo_route",
        "//   endpoints/health/health-route.rs -> mod endpoints_health_health_route",
        "//   endpoints/metrics/metrics-route.rs -> mod endpoints_metrics_metrics_route",
    ] {
        assert!(
            generated.contains(mapped),
            "the map must hold `{mapped}`:\n{generated}"
        );
    }
    // A Windows path has to arrive as an escaped literal; the backslash doubling is the
    // difference between a loadable module and a broken string escape.
    #[cfg(windows)]
    assert!(
        generated.contains(&format!(
            "#[path = \"{}\"]",
            echo.display().to_string().replace('\\', "\\\\")
        )),
        "the module path literal must double every backslash:\n{generated}"
    );
}

#[test]
fn an_empty_tree_still_generates_an_identity_router_and_an_empty_listing() {
    let fixture =
        Fixture::new("an_empty_tree_still_generates_an_identity_router_and_an_empty_listing");
    let report = fixture.scan();
    assert!(report.routes.is_empty());
    // No endpoint tree exists, so there is nothing to watch: the scan root is NOT a watch
    // path (T14/F1 - a recursive root watch would include the build's own output). A first
    // route therefore arrives through the watched discovery snapshot or
    // `TYPECRATE_SERVICES_RESCAN`, which is exactly the documented behaviour, and `build.rs`
    // warns about the empty tree with both remedies named.
    assert_eq!(fixture.watch(), Vec::<String>::new());
    assert_eq!(
        generated_body(&generate_module(&report)),
        "/// Registers every discovered service route onto `router`, in the order listed above.\n\
         pub fn register_routes(\n\
         \x20   router: axum::Router<crate::server::AppState>,\n\
         ) -> axum::Router<crate::server::AppState> {\n\
         \x20   router\n\
         }\n\n"
    );
    // R3 with ZERO modules: the listing still exists, still returns the contract type, and
    // its array literal is empty - so an empty tree keeps the whole host compiling with no
    // special case in `src/` and no `Vec::new()` fallback in the generator.
    let generated = generate_module(&report);
    assert_eq!(
        generated_metadata(&generated),
        "/// Metadata for every discovered service route, in the order listed above.\n\
         ///\n\
         /// Built-ins (`src/server`) are deliberately absent: this is the *discovered*\n\
         /// endpoint listing, and every row comes from the entries that module's\n\
         /// `crate::service_routes!` invocation already mounted, so it cannot disagree\n\
         /// with `register_routes` above.\n\
         pub fn route_metadata() -> Vec<crate::server::EndpointMeta> {\n\
         \x20   // `ENDPOINTS` of every discovered module, in registration order.\n\
         \x20   const DISCOVERED: &[&[crate::server::EndpointMeta]] = &[\n\
         \x20   ];\n\
         \x20   DISCOVERED.concat()\n\
         }\n"
    );
    // The empty tree gets no identifier map (there is nothing to map), so the header stays
    // honest instead of ending in a dangling label.
    assert!(!generated.contains("// Discovered route file -> generated module identifier:"));
    assert!(generated.contains("// Routes found: 0"));
}

#[test]
fn the_generated_endpoint_listing_names_the_same_modules_in_the_same_order() {
    // R3, structural half (the byte-for-byte half lives in the two tests above): a tree with
    // a nested alias directory, a nested route file and a base route file must produce a
    // registration chain and an endpoint listing that reference the SAME identifiers in the
    // SAME order. That equality is the whole "accurate metadata bound to the actual
    // registration" claim as seen from the generator.
    let fixture =
        Fixture::new("the_generated_endpoint_listing_names_the_same_modules_in_the_same_order");
    fixture
        .write("endpoints/health/health-route.rs", FIXTURE_ROUTE)
        .write("endpoints/echo/echo-route.rs", FIXTURE_ROUTE)
        // Written last, sorted first: creation order must not leak into either list.
        .write("endpoints/a/endpoint/nested-route.rs", FIXTURE_ROUTE);

    let generated = generate_module(&fixture.scan());
    let modules = [
        // Byte-wise order of the scan-root-relative paths: `endpoints/a/` (0x61) <
        // `endpoints/echo/` (0x65) < `endpoints/health/` (0x68).
        "endpoints_a_endpoint_nested_route",
        "endpoints_echo_echo_route",
        "endpoints_health_health_route",
    ];
    assert_eq!(
        referenced_modules(generated_body(&generated), "::register(router)"),
        modules
    );
    // The listing side must name `ENDPOINTS` through the same contract constant the
    // generator uses, so a rename on one side fails here rather than silently listing
    // nothing.
    assert_eq!(
        referenced_modules(
            generated_metadata(&generated),
            &format!("::{ROUTE_ENDPOINTS_CONST}")
        ),
        modules
    );

    // R3 serialisation safety, from the generator side: this section is what the playground
    // serves, so nothing path-shaped may appear in it. The only literals of any kind are the
    // doc comments, and they name Rust paths (`src/server`, `crate::service_routes!`), never
    // a file. (The value side is pinned in `src/server/metadata.rs`'s own tests, which assert
    // the exact JSON of one row.)
    let metadata = generated_metadata(&generated);
    // No string literal at all, therefore no `#[path = "..."]`-style value either.
    assert!(
        !metadata.contains('"'),
        "no string literal may be emitted:\n{metadata}"
    );
    // No path spelling: Windows separator, `.rs` suffix, the route-file name tail, or a
    // `C:/`-`https:/`-shaped colon (the colons that DO appear are `::` in Rust paths).
    assert!(
        !metadata.contains('\\'),
        "no Windows path separator may appear:\n{metadata}"
    );
    assert!(
        !metadata.contains(".rs"),
        "no route file name may leak:\n{metadata}"
    );
    assert!(
        !metadata.contains(ROUTE_FILE_SUFFIX),
        "no route file name tail may leak:\n{metadata}"
    );
    assert!(
        !metadata.contains("-route"),
        "identifiers are `_`-joined, so the hyphenated name tail may not leak:\n{metadata}"
    );
    assert!(
        !metadata.contains(":/") && !metadata.contains(":\\"),
        "the only colons must be `::` in Rust paths:\n{metadata}"
    );
    assert!(
        !metadata.contains(&fixture.root().display().to_string()),
        "the scan root must not reach the listed section:\n{metadata}"
    );
    // The type it returns is the shared DTO, named through the crate-scope re-export T18
    // owns (`src/server/mod.rs:42-43`), never a locally-declared struct.
    assert!(metadata.contains("pub fn route_metadata() -> Vec<crate::server::EndpointMeta> {"));
}

// ---------------------------------------------------------------------------
// T6: the route tree that lives in ANOTHER package of the repository.
// ---------------------------------------------------------------------------

/// The `/v1/harness` bridge (`packages/typecrate/harness/endpoint/harness/harness-route.rs`) is the
/// first route file discovered OUTSIDE this package, which is what makes the repository-root default
/// (R2) load-bearing rather than incidental: a scan rooted at `packages/typecrate/services` compiles
/// this crate happily and simply never mounts the harness.
///
/// Nothing here hardcodes the generated identifier - it is read out of the report and re-checked
/// against the naming rules. What is asserted about the generated text is that the header map, the
/// `#[path]` declaration, the `register_routes` chain and the `route_metadata` listing all name that
/// one module, in the scanner's order. The route file's *contents* stay out of reach by design (the
/// scanner never opens one), so the `/v1/harness` rows themselves are pinned over a socket by
/// `tests/server_http.rs` and `tests/harness_gateway.rs`.
#[test]
fn the_harness_bridge_in_another_package_is_discovered_and_registered() {
    // The scan-root-relative path, i.e. the sort key AND the identity this test is about.
    const HARNESS_ROUTE: &str = "packages/typecrate/harness/endpoint/harness/harness-route.rs";

    // The default root, resolved exactly as `build.rs` resolves it with no env override.
    let root =
        resolve_route_root(Path::new(MANIFEST_DIR), None).expect("the default root is usable");
    let report = scan_routes(&ScanOptions { root: root.clone() })
        .unwrap_or_else(|error| panic!("the repository root must scan cleanly: {error}"));

    let harness = report
        .routes
        .iter()
        .find(|route| route.relative_path == HARNESS_ROUTE)
        .unwrap_or_else(|| {
            panic!(
                "`{HARNESS_ROUTE}` is not discovered; the scan found {:?} under {}",
                report
                    .routes
                    .iter()
                    .map(|route| route.relative_path.as_str())
                    .collect::<Vec<_>>(),
                report.root.display()
            )
        });

    // The identifier is derived from the path, assigned for this report, and usable: `_`-joined,
    // `-route` tail collapsed to `_route`, and clean enough that the strict lint gate needs no
    // `#[allow(non_snake_case)]` on its declaration (F2).
    assert_valid_identifier(&harness.module_name);
    assert!(
        harness.module_name.ends_with("_route"),
        "the identifier keeps the `-route` half of its file name: {}",
        harness.module_name
    );
    assert!(
        !needs_snake_case_allow(&harness.module_name),
        "the generated declaration for `{}` would need a lint allow",
        harness.module_name
    );
    // The tree shape the rule needs: the singular `endpoint` alias, inside a sibling package, with
    // the exact `-route.rs` suffix; and the absolute path really is that file.
    assert_eq!(harness.root, root, "the report carries the root it scanned");
    assert!(
        harness.path.is_file(),
        "the discovered route file exists: {}",
        harness.path.display()
    );
    assert_eq!(
        relative_path(&root, &harness.path).replace('\\', "/"),
        HARNESS_ROUTE,
        "the report's relative path is the on-disk path from the scan root"
    );

    // Deterministic order (R1): `packages/typecrate/h…` sorts before `packages/typecrate/s…`, so the
    // bridge is registered FIRST - the exact order `tests/server_http.rs::route_table` pins.
    assert_eq!(report.routes[0].relative_path, HARNESS_ROUTE);
    let after_bridge: Vec<&str> = report
        .routes
        .iter()
        .skip(1)
        .map(|route| route.relative_path.as_str())
        .collect();
    assert_eq!(
        after_bridge,
        [
            "packages/typecrate/services/examples/endpoints/echo/echo-route.rs",
            "packages/typecrate/services/examples/endpoints/health/health-route.rs",
            "packages/typecrate/services/examples/endpoints/locations/locations-route.rs",
            "packages/typecrate/services/examples/endpoints/pets/pets-route.rs",
        ],
        "the bridge joined the sample set without reordering it"
    );

    // The generated module wires it up end to end.
    let generated = generate_module(&report);
    assert!(
        generated.contains(&format!(
            "//   {HARNESS_ROUTE} -> mod {}",
            harness.module_name
        )),
        "the header must map the discovered path to the identifier it generated"
    );
    assert!(
        generated.contains(&format!("\nmod {};", harness.module_name)),
        "the route file must get its own `#[path]` module declaration"
    );
    let expected: Vec<&str> = report
        .routes
        .iter()
        .map(|route| route.module_name.as_str())
        .collect();
    assert_eq!(
        referenced_modules(generated_body(&generated), "::register(router)"),
        expected,
        "`register_routes` must chain every discovered module, in order"
    );
    assert_eq!(
        referenced_modules(generated_metadata(&generated), "::ENDPOINTS"),
        expected,
        "`route_metadata` must list the same modules in the same order"
    );
}

#[test]
fn colliding_module_names_get_deterministic_numeric_suffixes() {
    let fixture = Fixture::new("colliding_module_names_get_deterministic_numeric_suffixes");
    // `-` and `_` are different on disk and identical inside an identifier; the first path
    // in registration order keeps the clean name, the second gets `_2`.
    fixture
        .write("endpoints/a-b/service-route.rs", FIXTURE_ROUTE)
        .write("endpoints/a_b/service-route.rs", FIXTURE_ROUTE)
        .write("endpoints/a__b/service-route.rs", FIXTURE_ROUTE);

    assert_eq!(
        fixture.routes(),
        [
            // `-` (0x2D) < `_` (0x5F), and `a_b` < `a__b` because `b` (0x62) > `_` (0x5F)
            // at the third segment, so the sorted order is a-b, a__b, a_b.
            "endpoints/a-b/service-route.rs|endpoints_a_b_service_route",
            "endpoints/a__b/service-route.rs|endpoints_a__b_service_route",
            "endpoints/a_b/service-route.rs|endpoints_a_b_service_route_2",
        ]
    );
}

#[test]
fn module_names_are_valid_identifiers_for_digit_keyword_and_punctuation_segments() {
    // The legacy bare spelling keeps working here too: its stem is `service`, so the
    // identifier is unchanged from the prefix era and a renamed example is the only thing
    // that moves an identifier.
    assert_eq!(
        module_name_for("endpoints/health/service-route.rs"),
        "endpoints_health_service_route"
    );
    // Only the `.rs` extension is stripped, never the `-route` half, so a discovered
    // identifier always ends in `_route` exactly like its file name ends in `-route.rs`.
    assert_eq!(
        module_name_for("endpoints/echo/echo-route.rs"),
        "endpoints_echo_echo_route"
    );
    assert_eq!(
        module_name_for("endpoints/locations/locations-route.rs"),
        "endpoints_locations_locations_route"
    );
    // A digit-leading segment gets a `_` prefix, hence the doubled separator.
    assert_eq!(
        module_name_for("endpoints/2024/health-route.rs"),
        "endpoints__2024_health_route"
    );
    assert_eq!(module_name_for("a b/health-route.rs"), "a_b_health_route");
    assert_eq!(module_name_for("a.b/health-route.rs"), "a_b_health_route");
    // A keyword is only reachable through a non-route path (`mod.rs` is not a route file),
    // but the guard is what keeps `mod <name>;` parseable.
    assert_eq!(module_name_for("type/health-route.rs"), "type_health_route");
    assert_eq!(module_name_for("mod.rs"), "_mod");
    // Degenerate input falls back to the route name tail minus its extension
    // (`-route` -> `_route`) instead of an empty identifier.
    assert_eq!(module_name_for(""), "_route");

    let names = assign_module_names(&[
        "endpoints/a-b/service-route.rs".to_owned(),
        "endpoints/a_b/service-route.rs".to_owned(),
        "endpoints/a__b/service-route.rs".to_owned(),
    ]);
    assert_eq!(
        names,
        [
            "endpoints_a_b_service_route",
            "endpoints_a_b_service_route_2",
            // `_` is preserved by the sanitizer, so this one never collided.
            "endpoints_a__b_service_route",
        ]
    );
    for name in &names {
        assert_valid_identifier(name);
    }
}

#[test]
fn snake_case_allow_is_needed_for_caps_and_doubled_separators_only() {
    // The measured rustc 1.99.1 matrix (`-D non-snake-case`, probed in
    // `Temp/opencode/t14/lint-probe.rs` / `lint-probe2.rs`), pinned here so the
    // generated-code predicate cannot drift from the compiler's rule: the first two forms
    // fail to compile under `npm run lint:strict`, the rest compile clean.
    for flagged in [
        "endpoints__2024_service_route",
        "endpoints_Unicode_service_route",
        "endpoints__n_code_service_route",
        "endpoints____service_route",
        "a__b",
        "a___b",
        "a__1_b",
    ] {
        assert!(
            needs_snake_case_allow(flagged),
            "`{flagged}` trips non_snake_case and must carry the allow"
        );
    }
    for clean in [
        "endpoints_2024_service_route",
        "endpoints_health_service_route",
        "endpoints_service_route_2",
        "a_2_b",
        "a_2b",
        "a2_b",
        "_a_b",
        "a_b_",
    ] {
        assert!(
            !needs_snake_case_allow(clean),
            "`{clean}` is snake case and must keep the full lint over its module body"
        );
    }
}

#[test]
fn digit_and_unicode_route_paths_generate_declarations_the_strict_lint_accepts() {
    let fixture =
        Fixture::new("digit_and_unicode_route_paths_generate_declarations_the_strict_lint_accepts");
    // The three path shapes that produce a *valid but not snake case* identifier, plus a
    // lowercase control that must stay untouched. Names are chosen so each expectation has
    // exactly one cause.
    for relative in [
        "endpoints/2024/service-route.rs",
        "endpoints/Unicode/service-route.rs",
        "endpoints/health/service-route.rs",
        "endpoints/\u{FC}n\u{EF}code/service-route.rs",
    ] {
        fixture.write(relative, FIXTURE_ROUTE);
    }

    let report = fixture.scan();
    // Registration order is byte-wise on the relative path: `2` (0x32) < `U` (0x55) <
    // `h` (0x68) < `\u{FC}` (0xC3 0xBC as UTF-8).
    assert_eq!(
        route_summaries(&report),
        [
            "endpoints/2024/service-route.rs|endpoints__2024_service_route",
            "endpoints/Unicode/service-route.rs|endpoints_Unicode_service_route",
            "endpoints/health/service-route.rs|endpoints_health_service_route",
            "endpoints/\u{FC}n\u{EF}code/service-route.rs|endpoints__n_code_service_route",
        ]
    );
    for route in &report.routes {
        assert_valid_identifier(&route.module_name);
    }

    // The emitted declarations, byte for byte: the allow rides in front of exactly the
    // three identifiers rustc rejects, and the lowercase control keeps its bare form.
    let generated = generate_module(&report);
    let declarations = generated
        .find("#[allow(non_snake_case)]")
        .map(|index| &generated[index..generated.find(REGISTER_MARKER).expect("registration doc")])
        .expect("a route that needs the allow must emit one");
    let path_of = |relative: &str| {
        // Component-wise join: the scanner yields platform-separated paths, and a single
        // `join("a/b/c")` on Windows would keep the `/` characters and compare unequal.
        let mut path = fixture.root().to_path_buf();
        for part in relative.split('/') {
            path.push(part);
        }
        path.display().to_string().escape_debug().to_string()
    };
    assert_eq!(
        declarations,
        format!(
            "#[allow(non_snake_case)]\n#[path = \"{digit}\"]\nmod endpoints__2024_service_route;\n\n\
             #[allow(non_snake_case)]\n#[path = \"{caps}\"]\nmod endpoints_Unicode_service_route;\n\n\
             #[path = \"{health}\"]\nmod endpoints_health_service_route;\n\n\
             #[allow(non_snake_case)]\n#[path = \"{unicode}\"]\nmod endpoints__n_code_service_route;\n\n",
            digit = path_of("endpoints/2024/service-route.rs"),
            caps = path_of("endpoints/Unicode/service-route.rs"),
            health = path_of("endpoints/health/service-route.rs"),
            unicode = path_of("endpoints/\u{FC}n\u{EF}code/service-route.rs"),
        )
    );
    // The chaining function needs no allow: rustc reports `non_snake_case` at the
    // definition, never at a use path (probe #1 emitted exactly one diagnostic per `mod`).
    assert_eq!(
        generated_body(&generated),
        "/// Registers every discovered service route onto `router`, in the order listed above.\n\
         pub fn register_routes(\n\
         \x20   router: axum::Router<crate::server::AppState>,\n\
         ) -> axum::Router<crate::server::AppState> {\n\
         \x20   let router = endpoints__2024_service_route::register(router);\n\
         \x20   let router = endpoints_Unicode_service_route::register(router);\n\
         \x20   let router = endpoints_health_service_route::register(router);\n\
         \x20   endpoints__n_code_service_route::register(router)\n\
         }\n\n"
    );
    // R3: the listing carries the same hostile identifiers (`_`-prefixed digit segment,
    // capitals, and the `_n_code` sanitising of non-ASCII) and needs no allow of its own -
    // the use path is not a definition, and `route_metadata`/`DISCOVERED` are snake case.
    let metadata = generated_metadata(&generated);
    assert_eq!(
        metadata,
        "/// Metadata for every discovered service route, in the order listed above.\n\
         ///\n\
         /// Built-ins (`src/server`) are deliberately absent: this is the *discovered*\n\
         /// endpoint listing, and every row comes from the entries that module's\n\
         /// `crate::service_routes!` invocation already mounted, so it cannot disagree\n\
         /// with `register_routes` above.\n\
         pub fn route_metadata() -> Vec<crate::server::EndpointMeta> {\n\
         \x20   // `ENDPOINTS` of every discovered module, in registration order.\n\
         \x20   const DISCOVERED: &[&[crate::server::EndpointMeta]] = &[\n\
         \x20       endpoints__2024_service_route::ENDPOINTS,\n\
         \x20       endpoints_Unicode_service_route::ENDPOINTS,\n\
         \x20       endpoints_health_service_route::ENDPOINTS,\n\
         \x20       endpoints__n_code_service_route::ENDPOINTS,\n\
         \x20   ];\n\
         \x20   DISCOVERED.concat()\n\
         }\n"
    );
    assert!(
        !metadata.contains("#[allow("),
        "the listing item must stay lint-clean:\n{metadata}"
    );
    // The mapping itself is unchanged: the fix is the attribute, not a renaming, so the
    // on-disk spelling (digit, capitals, non-ASCII) still survives into `#[path]`.
    assert!(generated.contains(&format!(
        "#[path = \"{}\"]",
        path_of("endpoints/Unicode/service-route.rs")
    )));
    assert!(
        generated
            .contains("endpoints/Unicode/service-route.rs -> mod endpoints_Unicode_service_route")
    );
}

#[test]
fn hyphenated_and_spaced_names_keep_their_paths_and_lose_only_identifiers() {
    let fixture =
        Fixture::new("hyphenated_and_spaced_names_keep_their_paths_and_lose_only_identifiers");
    fixture.write("endpoints/echo id/echo-route.rs", FIXTURE_ROUTE);

    let report = fixture.scan();
    assert_eq!(
        route_summaries(&report),
        ["endpoints/echo id/echo-route.rs|endpoints_echo_id_echo_route"]
    );
    // The on-disk name (hyphen, space) is what `#[path]` must carry; only the identifier is
    // sanitised.
    let on_disk = fixture
        .root()
        .join("endpoints")
        .join("echo id")
        .join("echo-route.rs");
    assert_eq!(report.routes[0].path, on_disk);
    let generated = generate_module(&report);
    assert!(generated.contains(&format!(
        "#[path = \"{}\"]",
        on_disk.display().to_string().escape_debug()
    )));
    assert!(generated.contains("mod endpoints_echo_id_echo_route;"));
}

// ---------------------------------------------------------------------------
// Cargo tracking (T14/F1): what re-runs the scan automatically, what does not.
// ---------------------------------------------------------------------------

#[test]
fn the_watch_set_covers_endpoint_trees_route_files_and_ancestor_ignore_files() {
    let fixture =
        Fixture::new("the_watch_set_covers_endpoint_trees_route_files_and_ancestor_ignore_files");
    fixture
        // Ancestor of the scan root's route tree: OUTSIDE every watched directory, so it is
        // watched as an individual file (editing it changes discovery).
        .write(".gitignore", "draft\n")
        .write("group/.ignore", "parked\n")
        // Inside the endpoint tree: already inside that tree's recursive directory watch, so
        // it is deliberately NOT listed again (this is what keeps the set narrow).
        .write("group/endpoints/.gitignore", "!draft-route.rs\n")
        .write("group/endpoints/health/.gitignore", "scratch\n")
        .write("group/endpoints/health/health-route.rs", FIXTURE_ROUTE)
        // An ordinary file inside the tree needs no entry of its own.
        .write("group/endpoints/health/notes.md", "not a route\n")
        .write("group/endpoints/health/health-core.rs", "not a route\n")
        // A route-shaped file outside any endpoint tree is neither a route nor a watch path.
        .write("src/health-route.rs", FIXTURE_ROUTE);

    // Exact list, sorted by scan-root-relative path: `.gitignore` (0x2E) first, then the
    // tree's own ancestor ignore file, the tree directory, and the route file. The `*-core.rs`
    // implementation file is absent because it is not a route, so editing it must not
    // re-run the scan (rustc's own dep-info still covers it).
    assert_eq!(
        fixture.watch(),
        [
            ".gitignore",
            "group/.ignore",
            "group/endpoints",
            "group/endpoints/health/health-route.rs",
        ]
    );
    let watch = fixture.watch();
    // The F1 loop guard: neither the scan root nor any directory that can contain build
    // output (`group`, the walk root's parent chain) is a watch path. A directory watch is
    // recursive in cargo, so one of these being watched is all it takes to make every
    // later `cargo` run dirty - the repository-wide default root has `.rust/target` in it.
    for not_watched in ["", ".", "group", "group/endpoints/health", "src"] {
        assert!(
            !watch.contains(&not_watched.to_owned()),
            "`{not_watched}` must not be a watch path: {watch:?}"
        );
    }
    // Only ignore files that EXIST are watched: a missing `rerun-if-changed` path is
    // permanently dirty (probed in T14), which would re-run the scan on every build.
    for missing in [
        ".ignore",
        ".git/info/exclude",
        "group/.gitignore",
        "group/.git/info/exclude",
        "group/endpoints/.ignore",
        "group/endpoints/health/.ignore",
    ] {
        assert!(
            !watch.contains(&missing.to_owned()),
            "`{missing}` does not exist and must not be watched: {watch:?}"
        );
    }
}

#[test]
fn nested_endpoint_aliases_produce_exactly_one_directory_watch() {
    let fixture = Fixture::new("nested_endpoint_aliases_produce_exactly_one_directory_watch");
    fixture
        // An alias inside an alias: the outer tree's recursive watch already covers it, so
        // the inner one must not add a second (slower, redundant) recursive watch.
        .write("endpoints/a/endpoint/a-route.rs", FIXTURE_ROUTE)
        .write("endpoints/b/b-route.rs", FIXTURE_ROUTE);

    assert_eq!(
        fixture.routes(),
        [
            "endpoints/a/endpoint/a-route.rs|endpoints_a_endpoint_a_route",
            "endpoints/b/b-route.rs|endpoints_b_b_route",
        ]
    );
    // Exactly one directory entry, `endpoints`, no matter how deep the aliases go.
    assert_eq!(
        fixture.watch(),
        [
            "endpoints",
            "endpoints/a/endpoint/a-route.rs",
            "endpoints/b/b-route.rs",
        ]
    );
}

#[test]
fn an_endpoint_tree_without_routes_contributes_no_watch_path() {
    let fixture = Fixture::new("an_endpoint_tree_without_routes_contributes_no_watch_path");
    // The tree exists but holds no route source, so nothing is discovered and nothing is
    // watched. That is the deliberate trade-off: watching every alias directory found in a
    // repository-wide walk would drag generated trees nobody cleans (an npm package's own
    // `node_modules` sitting inside `runtime/endpoint/`, for example) into the watch set.
    // The first route in such a tree arrives through the watched discovery snapshot or
    // `TYPECRATE_SERVICES_RESCAN` - both documented in README and repeated in the
    // build.rs "no route file found" warning.
    fixture
        .write("endpoints/notes.md", "no route here yet\n")
        // An implementation file and the pre-rename prefix spelling: both live in the tree and
        // both are ignored by the `*-route.rs` rule, which is the whole "a `*-core.rs` is never
        // discovered on its own" guarantee.
        .write("endpoints/health-core.rs", "implementation only\n")
        .write("endpoints/legacy/service-route-echo.rs", FIXTURE_ROUTE)
        // Right name, wrong place: outside every endpoint tree.
        .write("docs/health-route.rs", FIXTURE_ROUTE);

    assert_eq!(fixture.routes(), Vec::<String>::new());
    assert_eq!(fixture.watch(), Vec::<String>::new());
}

#[test]
fn a_new_route_tree_and_a_removed_one_both_change_the_report() {
    let fixture = Fixture::new("a_new_route_tree_and_a_removed_one_both_change_the_report");
    fixture.write("endpoints/health/health-route.rs", FIXTURE_ROUTE);
    let before = fixture.scan();
    assert_eq!(
        route_summaries(&before),
        ["endpoints/health/health-route.rs|endpoints_health_health_route"]
    );
    assert_eq!(
        before
            .watch_paths
            .iter()
            .map(|path| relative_path(fixture.root(), path))
            .collect::<Vec<String>>(),
        ["endpoints", "endpoints/health/health-route.rs"]
    );

    // A brand new directory plus route file inside an already watched tree: cargo's
    // recursive watch of `endpoints` fires for it (probed in T14: adding a file inside a
    // watched directory dirties the build script), so the only watch-set delta is the new
    // route file itself. The tree entry must stay present and identical - that is the
    // handle the new subdirectory is discovered through.
    fixture.write("endpoints/ops/ops-route.rs", FIXTURE_ROUTE);
    let after = fixture.scan();
    assert_eq!(
        route_summaries(&after),
        [
            "endpoints/health/health-route.rs|endpoints_health_health_route",
            "endpoints/ops/ops-route.rs|endpoints_ops_ops_route",
        ]
    );
    let tree = fixture.root().join("endpoints");
    assert!(before.watch_paths.contains(&tree) && after.watch_paths.contains(&tree));
    assert_eq!(
        after
            .watch_paths
            .iter()
            .map(|path| relative_path(fixture.root(), path))
            .collect::<Vec<String>>(),
        [
            "endpoints",
            "endpoints/health/health-route.rs",
            "endpoints/ops/ops-route.rs",
        ]
    );
    // The new *directory* is not itself listed: watching it separately would be a second
    // recursive watch of a subtree `endpoints` already covers.
    assert!(!after.watch_paths.contains(&tree.join("ops")));
    assert_ne!(before.watch_paths, after.watch_paths);

    // Removing it again returns the report to the original state, watch paths included.
    fs::remove_file(
        fixture
            .root()
            .join("endpoints")
            .join("ops")
            .join("ops-route.rs"),
    )
    .expect("remove fixture route");
    fs::remove_dir(fixture.root().join("endpoints").join("ops")).expect("remove fixture directory");
    assert_eq!(fixture.scan(), before);
    assert_eq!(
        fixture.watch(),
        ["endpoints", "endpoints/health/health-route.rs"]
    );
}

// ---------------------------------------------------------------------------
// Pure rules, asserted as complete matrices.
// ---------------------------------------------------------------------------

#[test]
fn the_name_rules_are_an_exact_matrix() {
    // Accepted: `<non-empty stem>-route.rs`, case-sensitive, stem free-form.
    for name in [
        "health-route.rs",
        "locations-route.rs",
        // A multi-hyphen stem is ordinary (`extra-pets` is the stem).
        "extra-pets-route.rs",
        // The tail is what counts, so an ordinary file that merely ends this way is a route.
        "notes-service-route.rs",
        "x-route.rs",
        // A dotted stem segment is fine as long as it is not a marker: the markers carry BOTH
        // dots, so `.test-` / `.spec-` are not markers.
        "pets.test-route.rs",
        // The legacy bare spelling: stem `service`, matches the pattern, no special case.
        "service-route.rs",
    ] {
        assert!(
            is_route_file_name(name),
            "`{name}` must be a route file name"
        );
    }
    for name in [
        // Not `-route.rs` at all.
        "index.rs",
        "mod.rs",
        "route.rs",
        "health-core.rs",
        "service-route-echo.rs",
        "service-route-pets.rs",
        "service-route-locations.rs",
        // Plural tail, and the old plural spelling.
        "pets-routes.rs",
        "service-routes.rs",
        // Empty stem: the stem is the half that names the resource.
        "-route.rs",
        // Wrong letter case, in the stem, in the tail and in the extension.
        "Pets-Route.rs",
        "HEALTH-ROUTE.rs",
        "health-route.RS",
        // Wrong or missing extension.
        "health-route.ts",
        "health-route.rs.tmp",
        "health-route",
        // A `.test.`/`.spec.` segment in the stem is a test source.
        "health.test.-route.rs",
        "pets.spec.-route.rs",
        "",
    ] {
        assert!(
            !is_route_file_name(name),
            "`{name}` must not be a route file name"
        );
    }

    for name in ENDPOINT_DIR_NAMES {
        assert!(
            is_endpoint_dir_name(name),
            "`{name}` must open a route tree"
        );
    }
    for name in [
        "Endpoint",
        "ENDPOINTS",
        "endpoints-old",
        "routes",
        "service-route",
        "",
    ] {
        assert!(
            !is_endpoint_dir_name(name),
            "`{name}` must not open a route tree"
        );
    }

    for name in [
        ".git",
        "node_modules",
        "target",
        ".rust",
        "Temp",
        "TEMP",
        "tmp",
        "TMP",
        "pkg",
        "dist",
        ".turbo",
    ] {
        assert!(is_pruned_dir_name(name), "`{name}` must be pruned by name");
    }
    // Generic names stay traversable: the repository `.gitignore` already ignores them, so
    // pruning them twice would only hide real route trees.
    for name in [
        "endpoints",
        "endpoint",
        "src",
        "lib",
        "app",
        "out",
        "build",
        "tests",
    ] {
        assert!(
            !is_pruned_dir_name(name),
            "`{name}` must not be pruned by name"
        );
    }
}

#[test]
fn the_tree_and_ancestor_helpers_are_exact_matrices() {
    // Pure path algebra (no filesystem), so the two helpers the watch set is built from are
    // pinned directly. `/repo` behaves identically on both platforms for these operations:
    // `strip_prefix`, `parent` and `join` are component based.
    let root = Path::new("/repo");
    let expect_dir = |relative: &str| {
        let mut path = root.to_path_buf();
        for part in relative.split('/') {
            path.push(part);
        }
        path
    };

    // The watch root is the OUTERMOST alias: a recursive directory watch of it already
    // covers every tree nested below, so nested aliases never add a second watch.
    assert_eq!(
        endpoint_tree_root(root, &expect_dir("endpoints/health/service-route.rs")),
        Some(expect_dir("endpoints"))
    );
    assert_eq!(
        endpoint_tree_root(root, &expect_dir("endpoint/service-route.rs")),
        Some(expect_dir("endpoint")),
        "a route directly inside the alias still has the alias as its tree"
    );
    assert_eq!(
        endpoint_tree_root(root, &expect_dir("endpoints/a/endpoint/service-route.rs")),
        Some(expect_dir("endpoints")),
        "an alias inside an alias resolves to the outer one"
    );
    assert_eq!(
        endpoint_tree_root(root, &expect_dir("a/endpoint/service-route.rs")),
        Some(expect_dir("a/endpoint")),
        "the alias is found at any depth below the root"
    );
    for outside in [
        "service-route.rs",
        "src/service-route.rs",
        "Endpoints/health/service-route.rs",
        "endpoints-old/health/service-route.rs",
    ] {
        assert_eq!(
            endpoint_tree_root(root, &expect_dir(outside)),
            None,
            "`{outside}` is not inside an endpoint tree"
        );
    }
    // The scan root is a boundary, not a tree, even when its own name is an alias.
    assert_eq!(
        endpoint_tree_root(
            &expect_dir("endpoints"),
            &expect_dir("endpoints/service-route.rs")
        ),
        None
    );
    // A file outside the root cannot have a tree inside it.
    assert_eq!(
        endpoint_tree_root(root, Path::new("/other/endpoints/health/service-route.rs")),
        None
    );

    // Ancestor chain: closest first, stopping AT the root (the root's own ignore files are
    // consulted by the walk; anything above it is not - `parents(false)`).
    assert_eq!(
        ancestor_dirs(root, &expect_dir("endpoints")),
        [root.to_path_buf()]
    );
    assert_eq!(
        ancestor_dirs(root, &expect_dir("a/b/endpoints")),
        [expect_dir("a/b"), expect_dir("a"), root.to_path_buf(),]
    );
    // Degenerate inputs are empty, never a panic and never a walk above the root.
    assert_eq!(ancestor_dirs(root, root), Vec::<PathBuf>::new());
    assert_eq!(
        ancestor_dirs(root, Path::new("/other/dir")),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn the_discovery_snapshot_is_the_repo_scratch_file_the_npm_pre_step_writes() {
    // Path contract with `scripts/discover-services.mjs`: repo root + SNAPSHOT_PATH. A
    // drift on either side silently disables automatic new-endpoint-folder pickup, so the
    // exact location is pinned, not just the segments.
    let manifest = Path::new(MANIFEST_DIR);
    let repo = repo_root();
    let snapshot = snapshot_file_path(manifest);
    let mut expected = repo.clone();
    for segment in SNAPSHOT_PATH {
        expected.push(segment);
    }
    assert_eq!(snapshot, expected);
    assert_eq!(
        relative_path(&repo, &snapshot),
        "Temp/typecrate-services/route-discovery.json"
    );
    // AGENTS.md: scratch lives in the repository-root `Temp/`, never in OS temp.
    assert!(snapshot.starts_with(repo.join("Temp")));
    // Not inside the package, which is where generated output (`pkg/`, `.rust/`) piles up,
    // and no segment of it is an endpoint alias - otherwise the watched snapshot would sit
    // inside a recursively watched tree and re-introduce the loop this design removes.
    assert!(!snapshot.starts_with(manifest));
    for segment in relative_path(&repo, &snapshot).split('/') {
        assert!(
            !is_endpoint_dir_name(segment),
            "the snapshot must live outside every endpoint tree, got `{segment}`"
        );
    }
}

#[test]
fn ensure_snapshot_file_seeds_once_and_never_rewrites_an_existing_file() {
    let fixture =
        Fixture::new("ensure_snapshot_file_seeds_once_and_never_rewrites_an_existing_file");
    let path = fixture
        .root()
        .join("nested")
        .join("deeper")
        .join("route.json");
    assert!(!path.exists());
    // Missing -> seeded, because cargo marks a build script permanently dirty when a
    // watched path does not exist (probed in T14), and `Temp/` may be cleared at any time.
    assert!(ensure_snapshot_file(&path), "a missing snapshot is seeded");
    assert_eq!(
        fs::read_to_string(&path).expect("seed read back"),
        SNAPSHOT_SEED
    );
    // Existing -> untouched. Rewriting the pre-step's bytes on every build would bump the
    // mtime and force a re-scan (and a recompile) on every single cargo invocation.
    fs::write(&path, "{\"revision\":\"from-the-npm-pre-step\"}\n").expect("pre-step bytes");
    assert!(ensure_snapshot_file(&path));
    assert_eq!(
        fs::read_to_string(&path).expect("read back after the pre-step"),
        "{\"revision\":\"from-the-npm-pre-step\"}\n"
    );
}

#[test]
fn the_generated_file_is_only_rewritten_when_its_bytes_change() {
    let fixture = Fixture::new("the_generated_file_is_only_rewritten_when_its_bytes_change");
    let path = fixture.root().join("out").join(GENERATED_FILE_NAME);
    assert!(write_generated_if_changed(&path, "one").expect("first write"));
    assert!(!write_generated_if_changed(&path, "one").expect("identical write"));
    assert!(write_generated_if_changed(&path, "two").expect("changed write"));
    assert_eq!(fs::read_to_string(&path).expect("read back"), "two");
    // A different file name in the same generated directory still works (the host picks the
    // name through `GENERATED_FILE_NAME`).
    let nested = fixture
        .root()
        .join("out")
        .join("other")
        .join(GENERATED_FILE_NAME);
    assert!(write_generated_if_changed(&nested, "three").expect("nested write"));
    assert_eq!(
        fs::read_to_string(&nested).expect("read back nested"),
        "three"
    );
}
