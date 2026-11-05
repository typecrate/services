//! The traversal: a gitignore-aware, deterministic walk that yields the route set plus the
//! paths cargo must watch.
//!
//! The *rules* (what counts as a route) live in [`crate::matching`]; this module only walks
//! and collects, which is what keeps the rules testable without a filesystem. The one
//! non-obvious part is the watch set ([`watch_set`]): cargo expands a directory watch
//! recursively and knows nothing about the scanner's ignore rules, so watching the scan root
//! would make every build dirty (the build's own artifacts live under that root).

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use ignore::WalkBuilder;

use super::DEFAULT_ROOT_ANCESTOR_STEPS;
use super::IGNORE_FILE_NAMES;
use super::ROUTE_ROOT_ENV;
use super::SNAPSHOT_PATH;
use super::SNAPSHOT_SEED;
use super::matching;
use super::naming;
use super::types::RouteSource;
use super::types::ScanError;
use super::types::ScanOptions;
use super::types::ScanReport;

/// Resolves the scan root: the compile-time override wins, otherwise the repository root
/// [`DEFAULT_ROOT_ANCESTOR_STEPS`] levels above `package_dir` (R2).
///
/// `configured` is the raw `TYPECRATE_SERVICES_ROUTE_ROOT` value: `None`, empty, or
/// whitespace-only means "use the default" (an empty value is how `set X=` on Windows and
/// `export X=` behave when someone wants the override *off*, and it must not become a
/// scan of the current directory). A relative override is resolved against `package_dir`,
/// so `.` means "this package" and `../../..` means "the repository root" (the default).
///
/// The result is lexically normalised (no `.`/`..` components, no filesystem access, no
/// symlink resolution - unlike `fs::canonicalize`, which also prepends `\\?\` on Windows
/// and would leak into the generated `#[path]` values).
///
/// # Errors
///
/// Returns an actionable message when the resolved root does not exist or is not a
/// directory - the two misconfiguration modes for an env override.
pub fn resolve_route_root(package_dir: &Path, configured: Option<&str>) -> Result<PathBuf, String> {
    let configured = configured.map(str::trim).filter(|value| !value.is_empty());
    let joined = match configured {
        None => ancestor_dir(package_dir, DEFAULT_ROOT_ANCESTOR_STEPS),
        Some(raw) => {
            let candidate = Path::new(raw);
            if candidate.is_absolute() {
                candidate.to_path_buf()
            } else {
                package_dir.join(candidate)
            }
        }
    };
    let root = normalize_lexically(&joined).ok_or_else(|| {
        format!(
            "`{ROUTE_ROOT_ENV}` resolved to `{}`, which has no absolute form",
            joined.display()
        )
    })?;
    if !root.is_dir() {
        let source = if configured.is_some() {
            format!("`{ROUTE_ROOT_ENV}`")
        } else {
            format!(
                "the default scan root (package dir `{}` + {DEFAULT_ROOT_ANCESTOR_STEPS} levels up)",
                package_dir.display()
            )
        };
        return Err(format!(
            "{source} resolved to `{root_path}`, which is not a directory. \
             Create it or point `{ROUTE_ROOT_ENV}` at the folder that holds your `endpoint`/`endpoints` trees.",
            root_path = root.display()
        ));
    }
    Ok(root)
}

/// Drops `DEFAULT_ROOT_ANCESTOR_STEPS` trailing components from an absolute path
/// (`packages/typecrate/services` -> repository root). Extra steps stop at the root.
fn ancestor_dir(path: &Path, steps: usize) -> PathBuf {
    let mut ancestor = path.to_path_buf();
    for _ in 0..steps {
        // `pop` returns false at a filesystem root; keeping the value is the sane answer
        // to an over-deep request rather than an error nobody can act on.
        if !ancestor.pop() {
            break;
        }
    }
    ancestor
}

/// Removes `.` and folds `..` components lexically; returns `None` when the path cannot be
/// made absolute (a relative path with no base - impossible here, both callers pass an
/// absolute base).
///
/// A `..` is only ever absorbed by a **normal** segment. At a filesystem root or drive
/// prefix (`/`, `D:\`) it is dropped, because `PathBuf::pop` there would delete the root
/// itself and turn `/a/../../b` into `b` instead of the `/b` the OS resolves; on a relative
/// path with nothing to pop the `..` is kept, because there is no base to resolve it against
/// yet. Same rules as `src/server/mod.rs::normalize_lexically`, so the two normalisers can
/// never disagree about where a root points.
fn normalize_lexically(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => match normalized.components().next_back() {
                // A real segment is what `..` cancels out.
                Some(std::path::Component::Normal(_)) => {
                    let _ = normalized.pop();
                }
                // Already at the top of an absolute path: `/..` == `/`, `D:\..` == `D:\`.
                Some(std::path::Component::Prefix(_)) | Some(std::path::Component::RootDir) => {}
                // Nothing to resolve against: keep the `..` (the path stays relative).
                _ => normalized.push(component.as_os_str()),
            },
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized.is_absolute().then_some(normalized)
}

/// Walks `options.root` and returns every `*-route.rs` inside an endpoint tree, in
/// deterministic registration order, together with cargo watch paths.
///
/// # Errors
///
/// Only a missing/non-directory scan root is fatal ([`ScanError`]); per-entry problems are
/// collected in [`ScanReport::errors`] so one unreadable corner of a repository-wide scan
/// cannot break the build.
pub fn scan_routes(options: &ScanOptions) -> Result<ScanReport, ScanError> {
    let root = options.root.as_path();
    if !root.is_dir() {
        return Err(ScanError {
            message: format!("scan root `{}` is not a directory", root.display()),
        });
    }

    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // `WalkBuilder::filter_entry` requires a `'static` closure, so the root is moved into
    // the predicate as an owned path instead of borrowing the `&Path` parameter.
    let filter_root = root.to_path_buf();
    let mut builder = WalkBuilder::new(root);
    builder
        // Ancestor ignore files are NOT consulted: the scan root is the boundary, so the
        // generated set does not depend on where the package sits inside a repository (and
        // a fixture under an ignored `Temp/` still works).
        .parents(false)
        // `.gitignore`/`.git/info/exclude` inside the tree are honoured even without a `.git`
        // anywhere (test fixtures), while the real repository behaves git-normal.
        .require_git(false)
        // The developer's global gitignore must not change a build output.
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .ignore(true)
        // Hidden trees (`.github/endpoints/`, `endpoints/.generated/`) are not route sources.
        .hidden(true)
        // Never follow a symlink: see `matching` docs + the symlink test.
        .follow_links(false)
        // Name-based pruning of dependency/build/scratch trees, independent of ignore rules.
        .filter_entry(move |entry| !is_pruned_directory(filter_root.as_path(), entry));

    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            // Non-fatal by design (permissions, a directory removed mid-walk).
            Err(err) => {
                errors.push(err.to_string());
                continue;
            }
        };
        let path = entry.path();
        let file_type = entry.file_type();

        // Directories are walked but never collected: the watch set is derived from the
        // *routes* (see [`watch_set`]), which is what keeps it away from the scan root and
        // from every generated-output ancestor. Ignore files are read by the `ignore` crate
        // during the walk, so nothing has to be probed here either.
        if !file_type.is_some_and(|kind| kind.is_file()) {
            continue;
        }
        // Symlinks (`file_type().is_symlink()`), sockets, devices and entries whose type
        // could not be determined are never routes: following a link could register one
        // file under two module names.
        if !matching::is_route_file_name(&matching::file_name_of(path)) {
            continue;
        }
        // The name rule alone is not enough: `src/health-route.rs` must stay a plain
        // module, only files inside an `endpoint`/`endpoints` tree are endpoints (R2).
        if !matching::inside_endpoint_tree(root, path) {
            continue;
        }
        found.push((matching::relative_path(root, path), path.to_path_buf()));
    }

    // Deterministic order, twice over: relative path first (platform independent), then
    // the absolute path as a tiebreak for the pathological same-relative-name case.
    found.sort_by(|(left_relative, left_path), (right_relative, right_path)| {
        left_relative
            .as_str()
            .cmp(right_relative.as_str())
            .then_with(|| left_path.cmp(right_path))
    });
    found.dedup_by(|left, right| left.1 == right.1);

    let module_names = naming::assign_module_names(
        &found
            .iter()
            .map(|(relative, _)| relative.clone())
            .collect::<Vec<String>>(),
    );
    let routes: Vec<RouteSource> = found
        .into_iter()
        .zip(module_names)
        .map(|((relative_path, path), module_name)| RouteSource {
            path,
            root: root.to_path_buf(),
            relative_path,
            module_name,
        })
        .collect();

    let watch_paths = watch_set(root, &routes);

    Ok(ScanReport {
        root: root.to_path_buf(),
        routes,
        watch_paths,
        errors,
    })
}

/// The cargo watch set for a finished scan - the narrowest set that still auto-detects the
/// changes that matter (T14/F1).
///
/// Cargo expands a `rerun-if-changed` **directory** recursively and applies none of the
/// scanner's ignore/prune knowledge to it (probed in T14: a file buried inside a watched
/// directory dirties the build script, and so does adding or deleting one). Watching the
/// scan root is therefore self-defeating: the build writes its own artifacts under the
/// repository root (`target-dir = .rust/target` in `<repo>/.cargo/config.toml`), so every
/// later cargo invocation sees a change *it* caused, re-runs this script, and recompiles
/// the crate forever - a build-script re-run recompiles even when `$OUT_DIR` bytes are
/// identical, which the probe also confirmed.
///
/// The set is instead:
///
/// * **the outermost `endpoint`/`endpoints` directory of every discovered route** - route
///   sources can only appear inside such a tree, and a recursive watch of it covers adding
///   a route file, adding/removing a route *subdirectory*, and editing/creating an ignore
///   file inside the tree;
/// * **each route file** - so a content edit is a generator input as well as a `rustc`
///   dep-info input for the `#[path]` module;
/// * **the ignore files above each watched tree** ([`crate::matching::ancestor_dirs`], as
///   individual files) - editing one changes discovery while touching no watched directory.
///
/// Not covered automatically (by design, documented in README): a **brand-new endpoint
/// tree** outside those directories, or a **newly created** ignore file above one. Both are
/// picked up by the npm discovery snapshot file (a single watched *file*, see
/// [`snapshot_file_path`], emitted by `build.rs`) or by the [`crate::RESCAN_ENV`] escape
/// hatch.
///
/// Only existing paths are emitted: a missing `rerun-if-changed` path is permanently dirty
/// (probed), which would re-run the scan on every single build.
fn watch_set(root: &Path, routes: &[RouteSource]) -> Vec<PathBuf> {
    let mut watch: BTreeSet<PathBuf> = BTreeSet::new();

    // One entry per route tree: `matching::endpoint_tree_root` always yields the outermost
    // alias, so every route in a tree maps to the same path and the set deduplicates.
    let trees: BTreeSet<PathBuf> = routes
        .iter()
        .filter_map(|route| matching::endpoint_tree_root(root, &route.path))
        .collect();
    for tree in &trees {
        watch.insert(tree.clone());
        // Existing ignore files strictly above the tree (the tree's own and everything below
        // it are already inside its recursive watch).
        for dir in matching::ancestor_dirs(root, tree) {
            for name in IGNORE_FILE_NAMES {
                let ignore_file = dir.join(name);
                if ignore_file.is_file() {
                    watch.insert(ignore_file);
                }
            }
        }
    }
    for route in routes {
        watch.insert(route.path.clone());
    }

    let mut watch_paths: Vec<PathBuf> = watch.into_iter().collect();
    // BTreeSet's `PathBuf` order is case-insensitive on Windows and case-sensitive on
    // POSIX; re-sorting by the `/`-normalised relative string makes the emitted
    // `cargo::rerun-if-changed` order identical everywhere.
    watch_paths.sort_by(|left, right| {
        matching::relative_path(root, left)
            .cmp(&matching::relative_path(root, right))
            .then_with(|| left.cmp(right))
    });
    watch_paths
}

/// Absolute path of the npm discovery snapshot for this package: the repository root
/// (`package_dir` + [`DEFAULT_ROOT_ANCESTOR_STEPS`] levels up) plus [`SNAPSHOT_PATH`].
///
/// Derived from the *package*, never from the scan root: the file is this package's rebuild
/// cache, and pointing `TYPECRATE_SERVICES_ROUTE_ROOT` somewhere else must not move it into
/// a tree the scanner walks (it stays under `Temp/`, a pruned directory name, either way).
#[must_use]
pub fn snapshot_file_path(package_dir: &Path) -> PathBuf {
    let mut path = ancestor_dir(package_dir, DEFAULT_ROOT_ANCESTOR_STEPS);
    for segment in SNAPSHOT_PATH {
        path.push(segment);
    }
    path
}

/// Creates the snapshot file with [`SNAPSHOT_SEED`] when it is missing, and reports whether
/// the path is a file afterwards (i.e. whether `build.rs` may watch it).
///
/// Cargo marks a build script permanently dirty when a watched path is missing (probed),
/// while `Temp/` is disposable scratch that may be cleared at any time - so seeding keeps
/// the watch stable. The content is never parsed by anyone: it exists so the file is there,
/// and a seed is what `scripts/discover-services.mjs` overwrites with real bytes on its next
/// run (one extra re-scan at that point, then stable again).
///
/// Everything here is best-effort on purpose: a scratch cache that cannot be written must
/// not fail a build, it only means "no automatic new-endpoint-tree pickup for this run".
pub fn ensure_snapshot_file(path: &Path) -> bool {
    if path.is_file() {
        return true;
    }
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return false;
    }
    // Two concurrent cargo runs racing on the seed is fine: identical bytes, last wins.
    let _ = fs::write(path, SNAPSHOT_SEED);
    path.is_file()
}

/// `filter_entry` predicate: skip pruned-by-name directories, never skip the scan root
/// itself, and never skip files (the ignore rules plus the name checks decide those).
fn is_pruned_directory(root: &Path, entry: &ignore::DirEntry) -> bool {
    if matching::is_scan_root(root, entry.path()) {
        return false;
    }
    // Only directories are name-pruned: a *file* called `target` or `dist` is ordinary.
    entry.file_type().is_some_and(|kind| kind.is_dir())
        && matching::is_pruned_dir_name(&matching::file_name_of(entry.path()))
}
