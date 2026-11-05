//! Build script: scans the tree for service routes at build time and writes
//! `$OUT_DIR/service_routes.rs`, the module the crate root includes.
//!
//! * Runs on the host only (cargo compiles build scripts for the host), so the `ignore`
//!   crate and filesystem walking can never reach the wasm artifact (R4). It still runs
//!   for a `--target wasm32-unknown-unknown` build, where the generated file is simply not
//!   included by anything: scanning anyway keeps one behaviour for every target and makes
//!   a misconfigured `TYPECRATE_SERVICES_ROUTE_ROOT` fail the same way everywhere.
//! * There is no runtime route registry: a discovered file is compiled in, so an invalid
//!   route is a native compile error pointing at that route file.
//! * Everything interesting lives in `build_support/` (shared with `tests/discovery.rs`);
//!   this file is only cargo plumbing: resolve, scan, write, report watch paths.
//! * The watch paths reported below are deliberately **narrow** (T14/F1). Cargo expands a
//!   `rerun-if-changed` directory *recursively* and applies none of the scanner's prune
//!   rules to it, so a watch on the scan root is a watch on the build's own output
//!   (`target-dir = .rust/target`): every later invocation would see a change the build
//!   itself caused, re-run this script, and recompile the crate - a build-script re-run
//!   recompiles even when the `$OUT_DIR` bytes are identical. See
//!   `build_support::discovery::watch_set` for what stays tracked automatically, and
//!   `build_support::RESCAN_ENV` / `build_support::SNAPSHOT_PATH` for the explicit re-scan.

mod build_support;

use std::path::PathBuf;
use std::process::ExitCode;

use build_support::GENERATED_FILE_NAME;
use build_support::RESCAN_ENV;
use build_support::ROUTE_ROOT_ENV;
use build_support::discovery::ensure_snapshot_file;
use build_support::discovery::resolve_route_root;
use build_support::discovery::scan_routes;
use build_support::discovery::snapshot_file_path;
use build_support::emit::generate_module;
use build_support::emit::write_generated_if_changed;
use build_support::types::ScanOptions;

fn main() -> ExitCode {
    // Cargo re-runs a build script when an env var it asked about changes; declaring the
    // override up front is what makes `set`/`unset` of it take effect even when no file
    // inside the previous root changed.
    println!("cargo::rerun-if-env-changed={ROUTE_ROOT_ENV}");
    // The explicit re-scan switch: any change of the value (including set -> unset) re-runs
    // the scan with no file edits and no `cargo clean`. It exists because the narrow file
    // watch set below cannot see a brand-new endpoint tree appear outside a watched tree.
    println!("cargo::rerun-if-env-changed={RESCAN_ENV}");

    // CARGO_MANIFEST_DIR is set while cargo *compiles* a build script, so it is baked in:
    // the default root is this package's directory, three levels up = repository root (R2).
    let package_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let configured = read_env(ROUTE_ROOT_ENV);
    let root = match resolve_route_root(&package_dir, configured.as_deref()) {
        Ok(root) => root,
        Err(message) => return fail(&message),
    };

    let report = match scan_routes(&ScanOptions { root }) {
        Ok(report) => report,
        Err(error) => return fail(&format!("service-route scan failed: {error}")),
    };

    // OUT_DIR is documented as available at build-script *runtime* (that is the guaranteed
    // spelling; the compile-time env! only exists for the script's own compilation).
    let out_dir = match read_env("OUT_DIR") {
        Some(out_dir) => PathBuf::from(out_dir),
        None => {
            return fail(
                "OUT_DIR is not set, so the generated service-route module has nowhere to go.",
            );
        }
    };
    let generated_path = out_dir.join(GENERATED_FILE_NAME);
    match write_generated_if_changed(&generated_path, &generate_module(&report)) {
        Ok(_) => {}
        Err(message) => return fail(&message),
    }

    // What this re-runs the scan for: adding/changing/removing a route file, adding or
    // removing a route subdirectory, and editing an ignore file inside a discovered endpoint
    // tree (that tree is a watched directory, and cargo's directory watch is recursive) or
    // above one (those are watched as individual files). Nothing here is an ancestor of
    // `$OUT_DIR`, which is the whole point - see the module docs.
    for path in &report.watch_paths {
        // `display()` is lossy for non-UTF-8 paths; a repository whose directory names are
        // not UTF-8 decodable cannot be scanned reliably by any generator, so the lossy
        // form is reported rather than skipped.
        println!("cargo::rerun-if-changed={}", path.display());
    }

    // The npm discovery snapshot (`scripts/discover-services.mjs`) is how a *new endpoint
    // folder* outside those trees gets picked up without a manual trigger: the script
    // rewrites this single file when the tree shape changes, and a file watch is not
    // recursive, so it can never pull a generated-output tree into the dependency set.
    // It is seeded when missing because a missing watched path makes cargo re-run this
    // script on every build (probed), and `Temp/` is disposable scratch.
    let snapshot = snapshot_file_path(&package_dir);
    if ensure_snapshot_file(&snapshot) {
        println!("cargo::rerun-if-changed={}", snapshot.display());
    }

    // One unreadable corner of a repository-wide walk must not break the build, but it must
    // not be silent either.
    for error in &report.errors {
        println!("cargo::warning=service-route scan skipped an entry: {error}");
    }
    if report.routes.is_empty() {
        println!(
            "cargo::warning=no `*{suffix}` route file was found under `{root}`; `register_routes` registers \
             nothing. Point {ROUTE_ROOT_ENV} at the folder holding your `endpoint`/`endpoints` trees, or set \
             {RESCAN_ENV} to any new value (`npm run discover` does the equivalent) after adding a new one.",
            suffix = build_support::ROUTE_FILE_SUFFIX,
            root = report.root.display()
        );
    }
    ExitCode::SUCCESS
}

/// Reads an env var, treating an empty or whitespace-only value as unset: `set X=` on
/// Windows and `export X=` both leave an empty string, and that has to mean "default".
fn read_env(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?;
    (!value.trim().is_empty()).then_some(value)
}

/// Emits one actionable cargo error and stops the build. The `cargo::error=` key (stable
/// since cargo 1.77, and this workspace's rust-version is 1.99) is the documented channel
/// for a build script's own fatal message, so the developer reads the fix rather than a
/// stack trace.
fn fail(message: &str) -> ExitCode {
    println!("cargo::error=typecrate-services build.rs: {message}");
    ExitCode::FAILURE
}
