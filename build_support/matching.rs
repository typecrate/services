//! The matching rules: which directory opens a route tree, which file name is a route,
//! which directory is never descended into, and how paths are rendered for output.
//!
//! No filesystem access and no `ignore` dependency here, so every rule is directly
//! assertable (see `tests/discovery.rs`, which asserts the exact accept/reject matrix).

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use super::ENDPOINT_DIR_NAMES;
use super::PRUNED_DIR_NAMES;
use super::ROUTE_FILE_SUFFIX;
use super::TEST_NAME_MARKERS;

/// `true` when `name` (a bare directory name) is one of the endpoint tree aliases.
///
/// Exact and case-sensitive: the contract fixes exactly `endpoint` and `endpoints`.
#[must_use]
pub fn is_endpoint_dir_name(name: &str) -> bool {
    ENDPOINT_DIR_NAMES.contains(&name)
}

/// `true` when a directory called `name` must be skipped regardless of ignore rules
/// (dependency, build-output, tool-cache and scratch trees).
///
/// Case-insensitive on purpose: on a case-insensitive filesystem `Temp/` and `TEMP/` are
/// the same scratch tree, and missing one spelling would register scratch routes.
#[must_use]
pub fn is_pruned_dir_name(name: &str) -> bool {
    PRUNED_DIR_NAMES
        .iter()
        .any(|pruned| name.eq_ignore_ascii_case(pruned))
}

/// `true` when `file_name` is a route source: it ends with `-route.rs`, the stem before that
/// suffix is non-empty, and the stem carries no `.test.`/`.spec.` segment.
///
/// Accepted: `health-route.rs`, `locations-route.rs`, `extra-pets-route.rs` (a multi-hyphen
/// stem is ordinary), `notes-service-route.rs` (the tail is what counts), and the legacy bare
/// `service-route.rs` - its stem is `service`, so it matches the pattern and stays discovered
/// (no special case; README still tells authors to rename it).
///
/// Rejected: the old `service-route*` **prefix** spellings (`service-route-echo.rs`, the
/// migration hazard - it drops out of the build silently), the plural `pets-routes.rs`, an
/// implementation file `pets-core.rs`, `mod.rs`, a wrong letter case (`Pets-Route.rs`) or
/// extension (`health-route.RS`, `health-route.ts`, `health-route.rs.tmp`), the empty-stem
/// `-route.rs`, and every stem carrying a `.test.`/`.spec.` segment.
#[must_use]
pub fn is_route_file_name(file_name: &str) -> bool {
    // `strip_suffix` is a byte-exact tail test, so it decides the extension AND the letter
    // case in one step: `health-route.RS`, `health-route.ts` and `Pets-Route.rs` simply have
    // no `-route.rs` tail. The only accepted spelling is `<stem>-route.rs`.
    let Some(stem) = file_name.strip_suffix(ROUTE_FILE_SUFFIX) else {
        return false;
    };
    // The stem is the half that names the resource (and the half the generated module
    // identifier is built from), so a bare `-route.rs` is not a route.
    if stem.is_empty() {
        return false;
    }
    // A test marker anywhere in the stem wins, e.g. `pets.spec.-route.rs`. Checked on the
    // stem: a marker needs two dots and [`ROUTE_FILE_SUFFIX`] contributes only the one in
    // `.rs` (the four bytes before it are `oute`, never `test`/`spec`), so no marker can
    // straddle the boundary and the stem test and a whole-name test agree exactly.
    !TEST_NAME_MARKERS.iter().any(|marker| stem.contains(marker))
}

/// `true` when some directory between `root` and `file` is an endpoint tree alias, i.e.
/// the file lives *inside* an `endpoint`/`endpoints` folder tree (R2).
///
/// The file name itself is never an alias candidate (only directory components count), and
/// a route file sitting outside such a tree is never registered even though its name
/// matches - that is what keeps `src/health-route.rs` from becoming an endpoint.
#[must_use]
pub fn inside_endpoint_tree(root: &Path, file: &Path) -> bool {
    endpoint_tree_root(root, file).is_some()
}

/// The **outermost** `endpoint`/`endpoints` directory that contains `file`, or `None` when
/// `file` is outside every endpoint tree (the boolean form is [`inside_endpoint_tree`]).
///
/// Why "outermost" and not "nearest": this is the path `build.rs` hands cargo as a watch
/// root, and cargo watches a directory recursively, so the outer alias already covers every
/// tree nested below it (`endpoints/x/endpoint/` is inside the watch of `endpoints/`).
/// Always returning the outermost alias is also what makes the watch set free of nested
/// duplicates without a second filtering pass: two routes in one tree - at any depth, alias
/// or not - produce the same value, so a set deduplicates them.
///
/// Only components *below* `root` are candidates, matching [`inside_endpoint_tree`]: a scan
/// root named `endpoints` is a boundary, not a route tree (the contract is "inside").
#[must_use]
pub fn endpoint_tree_root(root: &Path, file: &Path) -> Option<PathBuf> {
    let relative = file.strip_prefix(root).ok()?;
    // The alias must be a *directory* on the way to the file, so the file name itself is
    // excluded before the components are walked.
    let parent = relative.parent()?;
    let mut outermost: Option<PathBuf> = None;
    let mut current = PathBuf::new();
    for component in parent.components() {
        // `..`/`.`/prefix components cannot be alias directories, and the *first* alias
        // wins: it is the outermost one, which is the subtree cargo should watch.
        if let Component::Normal(name) = component {
            current.push(name);
            if outermost.is_none() && is_endpoint_dir_name(&name.to_string_lossy()) {
                outermost = Some(current.clone());
            }
        }
    }
    outermost.map(|relative_dir| root.join(relative_dir))
}

/// Every directory from `root` down to (but not including) `dir`, closest first.
///
/// Used to find the ignore files that sit *above* a watched endpoint tree: they change what
/// the scan discovers, but they live outside every watched directory, so they have to be
/// watched as individual files. `root` itself is included (its own `.gitignore` is read by
/// the walk); ignore files above `root` are not read at all (`parents(false)`).
///
/// Returns an empty list when `dir` is `root` or sits outside it: there is then no chain of
/// directories the scanner consulted, and never an error.
#[must_use]
pub fn ancestor_dirs(root: &Path, dir: &Path) -> Vec<PathBuf> {
    if dir == root || !dir.starts_with(root) {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut current = dir.parent().map(PathBuf::from);
    while let Some(parent) = current {
        // `PathBuf::parent()` on an absolute path walks past the root into `None`, so the
        // scan root is the stop condition rather than relying on that.
        if parent == root {
            dirs.push(parent.clone());
            break;
        }
        dirs.push(parent.clone());
        current = parent.parent().map(PathBuf::from);
    }
    dirs
}

/// Base name of `path` as a `String` (empty for paths without a file name, e.g. `/`).
#[must_use]
pub fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map_or(String::new(), |name| name.to_string_lossy().into_owned())
}

/// `path` rendered relative to `root` with `/` separators, so ordering, comments and the
/// generated module identifiers are byte-identical on Windows and POSIX.
///
/// Falls back to `path` itself when it is not under `root` (only affects diagnostics and
/// the watch-path sort, never the route set).
#[must_use]
pub fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    // Components, not `Path::strip` string tricks: this normalises `/` vs `\` and drops
    // `.` entries, which is what makes the comparison platform independent.
    let parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.join("/")
}

/// `true` when `path` is `root` itself (the walk's depth-0 entry). The scan root is never
/// pruned by name: a caller may legitimately point `TYPECRATE_SERVICES_ROUTE_ROOT` at a
/// directory whose name is in `PRUNED_DIR_NAMES`.
#[must_use]
pub fn is_scan_root(root: &Path, path: &Path) -> bool {
    path == root
}
