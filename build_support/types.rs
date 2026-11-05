//! Data carried between the scanner, the generator and `build.rs`.
//!
//! Split out from `discovery.rs` on purpose: this module has no `ignore` dependency, so
//! the generation rules can be exercised (and are exercised, in `Temp/` probes and
//! `tests/discovery.rs`) without a filesystem walk.

use std::fmt;
use std::path::PathBuf;

/// One discovered route file plus the module identifier it is compiled under.
///
/// `PartialEq`/`Eq` are derived so tests can assert whole reports, not field glimpses.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteSource {
    /// Absolute path of the route file as it was found on disk.
    pub path: PathBuf,
    /// Absolute scan root this route was discovered under.
    pub root: PathBuf,
    /// `path` relative to `root`, `/`-normalised. This is what the generated comments and
    /// the deterministic sort order are built from, and it is identical on every platform.
    pub relative_path: String,
    /// Generated module identifier (sanitised and already made unique for this report).
    pub module_name: String,
}

/// Input to [`crate::scan_routes`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanOptions {
    /// Directory the scan starts at. It must exist and be a directory; it is never
    /// pruned by name even if it is e.g. `Temp/`.
    pub root: PathBuf,
}

/// Everything a scan produced: the routes, plus the paths `build.rs` must hand to cargo
/// so the next edit re-runs the scan.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanReport {
    /// Absolute scan root, echoed for the generated header and for tests.
    pub root: PathBuf,
    /// Discovered routes in **registration order** (deterministic: byte-wise comparison
    /// of [`RouteSource::relative_path`], tie-broken by the absolute path).
    pub routes: Vec<RouteSource>,
    /// Paths for `cargo::rerun-if-changed`: the outermost `endpoint`/`endpoints` directory of
    /// every discovered route (recursively watched by cargo), every route file itself, and
    /// every ignore file that exists *above* one of those directories. Sorted by
    /// scan-root-relative path so the emitted directive order is reproducible.
    ///
    /// Why these three kinds (and nothing broader - see `discovery.rs::watch_set`, because a
    /// directory watch is recursive and cargo ignores the scanner's prune rules):
    /// * a watched endpoint tree's recursive watch fires when anything is added, changed or
    ///   removed inside it -> new/removed route files, new route subdirectories and ignore
    ///   edits inside the tree all re-run the scan;
    /// * an ignore file *above* the tree changes discovery without touching any watched path,
    ///   so the existing ones are watched as individual files (non-recursive);
    /// * a route file's *content* change is picked up by rustc's own dep-info (the `#[path]`
    ///   module) and is re-watched here so the generator stays honest.
    ///
    /// The scan root itself is deliberately absent: the build's own artifacts live under it,
    /// and a recursive watch of it makes every cargo invocation dirty. Newly created
    /// endpoint trees / ignore files outside the watched trees are the documented blind spot
    /// and are covered by the watched snapshot file plus `TYPECRATE_SERVICES_RESCAN`.
    ///
    /// Only paths that exist are listed: cargo treats a missing `rerun-if-changed` path as
    /// perpetually dirty, which would re-run the scan on every build.
    pub watch_paths: Vec<PathBuf>,
    /// Per-entry traversal problems (permission denied, a directory deleted mid-walk).
    /// Non-fatal by design: one unreadable corner of a repository-wide walk must not
    /// break a build. `build.rs` surfaces each one as a `cargo::warning`.
    pub errors: Vec<String>,
}

/// Fatal scan failure (a bad scan root). Everything below that is reported through
/// [`ScanReport::errors`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanError {
    /// Actionable, user-facing message; `build.rs` prints it as a cargo error.
    pub message: String,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ScanError {}
