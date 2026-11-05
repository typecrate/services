// =============================================================================
// discover-services.mjs - the discovery pre-step for the native cargo scripts.
//
// WHY this exists (T14/F1): cargo's only directory watch is *recursive*, and it
// ignores the scanner's prune rules, so `build.rs` cannot watch the scan root -
// the build's own artifacts live under it (repo-root `.cargo/config.toml` sets
// `target-dir = .rust/target`), which made every cargo invocation dirty and
// recompiled the crate forever. The watch set is therefore scoped to the
// endpoint trees that produced routes (see
// `build_support/discovery.rs::watch_set`), and what that set structurally
// cannot see is a **brand-new `endpoint`/`endpoints` folder** appearing outside
// an already watched tree.
//
// This script closes that gap with no daemon and no dependency: it walks the
// tree with the same prune rules the scanner uses and writes ONE file -
//   <repo>/Temp/typecrate-services/route-discovery.json
// which `build.rs` watches as an individual path. A file watch is not recursive,
// so it can never drag generated output back into the dependency set.
//
// Contract with build.rs (both sides are pinned by tests/discovery.rs):
//   * snapshot path == build_support::discovery::snapshot_file_path()
//                      (repository root == package dir + 3 ancestors)
//   * watch         == `cargo::rerun-if-changed=<that file>`, emitted only when
//                      the file exists (a missing watch path is permanently
//                      dirty, which cargo itself confirmed in the T14 probe)
//   * manual switch == `cargo::rerun-if-env-changed=TYPECRATE_SERVICES_RESCAN`
//                      (`TYPECRATE_SERVICES_RESCAN=<any new value> cargo check`
//                      forces a re-scan without touching any file)
//
// Deliberately conservative and deliberately cheap:
//   * it does NOT evaluate gitignore rules (that would mean a second gitignore
//     implementation): the snapshot is a superset, so an extra path appearing can
//     at worst cost one harmless extra re-scan, never miss one;
//   * the revision hashes *paths only*, so editing a route file's contents never
//     re-runs the scan (rustc's own dep-info covers content edits);
//   * the file is rewritten only when its bytes change, because cargo compares
//     mtimes: rewriting identical bytes would rebuild on every single run.
//
// Flags: --force  rewrite the snapshot unconditionally (mtime bump => cargo
//                re-runs the scan; this is the documented manual trigger);
//        --print  also print the snapshot JSON to stdout (inspection / CI).
// =============================================================================

import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
/** The npm package == the Cargo workspace root (single combined crate). */
const packageDir = resolve(here, "..");
const LOG = "[typecrate-services:discover]";

// ----------------------------------------------------------------- constants
// Mirrors of `build_support/mod.rs`: these are the whole contract with the Rust
// side. `tests/discovery.rs` pins the Rust spellings, so a rename on either
// side becomes a test failure instead of a silently stale route table.
const ROUTE_ROOT_ENV = "TYPECRATE_SERVICES_ROUTE_ROOT";
const RESCAN_ENV = "TYPECRATE_SERVICES_RESCAN";
/** `DEFAULT_ROOT_ANCESTOR_STEPS`: this package sits at `<repo>/packages/<scope>/<name>`. */
const ROOT_ANCESTOR_STEPS = 3;
/** `SNAPSHOT_PATH`: segments below the repository root (git-ignored scratch). */
const SNAPSHOT_SEGMENTS = ["Temp", "typecrate-services", "route-discovery.json"];
/** `ROUTE_FILE_SUFFIX` (the whole tail, extension included) / `TEST_NAME_MARKERS`. */
const ROUTE_SUFFIX = "-route.rs";
const TEST_MARKERS = [".test.", ".spec."];
/** `ENDPOINT_DIR_NAMES`: exact and case-sensitive, like the scanner. */
const ENDPOINT_DIRS = new Set(["endpoint", "endpoints"]);
/** `IGNORE_FILE_NAMES` (`.git/info/exclude` is handled separately: it is nested). */
const IGNORE_FILES = new Set([".gitignore", ".ignore"]);
const GIT_EXCLUDE = ".git/info/exclude";
/** `PRUNED_DIR_NAMES`, matched case-insensitively. */
const PRUNED_DIRS = new Set([
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
]);

// ------------------------------------------------------------------ utilities
const args = process.argv.slice(2);
const force = args.includes("--force");
const print = args.includes("--print");

/** Repository root: package dir + `ROOT_ANCESTOR_STEPS` levels up (no fs access). */
function ancestorDir(start, steps) {
    let dir = start;
    for (let i = 0; i < steps; i += 1) dir = dirname(dir);
    return dir;
}

/** Same empty/whitespace-is-unset rule `build.rs::read_env` applies. */
function envValue(name) {
    const value = process.env[name];
    if (value === undefined || value.trim() === "") return undefined;
    return value.trim();
}

/** Scan root: the override wins, otherwise the repository root (R2). */
function resolveScanRoot() {
    const fallback = ancestorDir(packageDir, ROOT_ANCESTOR_STEPS);
    const configured = envValue(ROUTE_ROOT_ENV);
    if (configured === undefined) return fallback;
    // A relative override resolves against the package dir, like
    // `discovery::resolve_route_root`.
    return isAbsolute(configured) ? resolve(configured) : resolve(packageDir, configured);
}

const scanRoot = resolveScanRoot();
const repoRoot = ancestorDir(packageDir, ROOT_ANCESTOR_STEPS);
const snapshotPath = SNAPSHOT_SEGMENTS.reduce((path, segment) => join(path, segment), repoRoot);

const baseName = (path) => path.split(/[/\\]/).pop() ?? path;

/** `path` relative to the scan root, `/`-separated ("" == the scan root itself). */
function relativeFromRoot(path) {
    const rel = relative(scanRoot, path);
    return rel === "" ? "" : rel.split(sep).join("/");
}

/**
 * Route file name rule - this mirrors `build_support/matching.rs::is_route_file_name`
 * exactly and MUST stay in sync with it: the snapshot's route list is what makes the
 * printed counts agree with the table `build.rs` generates. A route file is
 * `<non-empty stem>-route.rs` whose stem carries no `.test.`/`.spec.` segment; the
 * comparison is case-sensitive and the whole tail (extension included) must match, so
 * `pets-routes.rs`, `Pets-Route.rs`, `pets-core.rs`, `mod.rs`, `-route.rs` and the legacy
 * `service-route-echo.rs` spellings are all ignored here, exactly as they are by cargo.
 */
function isRouteFileName(name) {
    if (!name.endsWith(ROUTE_SUFFIX)) return false;
    // The stem is everything before the fixed tail; "" means the bare `-route.rs`.
    const stem = name.slice(0, name.length - ROUTE_SUFFIX.length);
    if (stem === "") return false;
    return !TEST_MARKERS.some((marker) => stem.includes(marker));
}

// ----------------------------------------------------------------------- walk
// Iterative (no recursion limit on deep trees) and symlink-blind, matching
// `WalkBuilder::follow_links(false)`: following a link could double-count a tree.
function collectSnapshot() {
    const endpointDirs = new Set();
    const routeFiles = new Set();
    const ignoreFiles = new Set();
    // `[directory, insideEndpointTree]`; the scan root is a boundary, never a tree.
    const stack = [[scanRoot, false]];
    while (stack.length > 0) {
        const [dir, insideEndpoint] = stack.pop();
        // `inside_endpoint_tree` is decided by *ancestor* components, so the flag
        // is computed before descending and carried down.
        const childInside = insideEndpoint || ENDPOINT_DIRS.has(baseName(dir));
        let entries;
        try {
            entries = readdirSync(dir, { withFileTypes: true });
        } catch {
            // Unreadable directory: the scanner records it as a non-fatal error and
            // continues, and so does this snapshot pass.
            continue;
        }
        for (const entry of entries) {
            const name = entry.name;
            const full = join(dir, name);
            if (entry.isDirectory()) {
                // Pruned/generated/hidden/linked trees are not scanned by cargo's
                // generator either, so they must not appear in the snapshot.
                if (PRUNED_DIRS.has(name.toLowerCase())) continue;
                if (name.startsWith(".") || entry.isSymbolicLink()) continue;
                if (ENDPOINT_DIRS.has(name)) endpointDirs.add(relativeFromRoot(full));
                stack.push([full, childInside]);
                continue;
            }
            if (entry.isSymbolicLink()) continue;
            if (IGNORE_FILES.has(name)) {
                // Ignore files count wherever they are: creating one can change
                // discovery, and only existing ones can be watched by cargo.
                ignoreFiles.add(relativeFromRoot(full));
                continue;
            }
            if (!childInside || name.startsWith(".")) continue;
            if (isRouteFileName(name)) routeFiles.add(relativeFromRoot(full));
        }
        // `<dir>/.git/info/exclude` is part of the scanner's ignore set at every
        // level (it exists only where a `.git` lives, i.e. normally the repo root).
        if (existsSync(join(dir, GIT_EXCLUDE))) {
            const rel = relativeFromRoot(dir);
            ignoreFiles.add(rel === "" ? GIT_EXCLUDE : `${rel}/${GIT_EXCLUDE}`);
        }
    }
    const sorted = (set) => [...set].sort();
    const trees = sorted(endpointDirs);
    const files = sorted(routeFiles);
    const ignores = sorted(ignoreFiles);
    // Paths only, in a fixed order: a stable tree must produce a stable revision,
    // otherwise the pre-step would rebuild on every run.
    const payload = ["v1", ...trees, ...files, ...ignores].join("\n");
    return {
        revision: createHash("sha256").update(payload).digest("hex"),
        scanRoot,
        endpointDirs: trees,
        routeFiles: files,
        ignoreFiles: ignores,
    };
}

// ------------------------------------------------------------------- writing
// Write-if-changed: cargo's watch is an mtime comparison, so touching the file
// on every run would rebuild on every run.
function publish(data) {
    const content = `${JSON.stringify(data, null, 2)}\n`;
    let previous;
    try {
        previous = readFileSync(snapshotPath, "utf8");
    } catch {
        previous = undefined;
    }
    if (!force && previous === content) return false;
    mkdirSync(dirname(snapshotPath), { recursive: true });
    writeFileSync(snapshotPath, content);
    return true;
}

function main() {
    let rootStat;
    try {
        rootStat = statSync(scanRoot);
    } catch {
        // A missing root is `build.rs`'s error to report (its message names
        // TYPECRATE_SERVICES_ROUTE_ROOT and what to create), so this step must not
        // preempt it with a worse one; exiting 0 keeps the cargo script running.
        console.error(`${LOG} scan root ${scanRoot} is not readable - no discovery snapshot written (cargo reports the scan error)`);
        return 0;
    }
    if (!rootStat.isDirectory()) {
        console.error(`${LOG} scan root ${scanRoot} is not a directory - no discovery snapshot written`);
        return 0;
    }
    const data = collectSnapshot();
    const written = publish(data);
    if (print) process.stdout.write(`${JSON.stringify(data, null, 2)}\n`);
    if (written) {
        // Only a changed tree has something to say; a no-op run stays silent so
        // `npm start` output does not change.
        console.log(`${LOG} snapshot ${force ? "rewritten (--force)" : "changed"}: ${data.endpointDirs.length} endpoint tree(s), ` +
            `${data.routeFiles.length} route file(s), ${data.ignoreFiles.length} ignore file(s) under ${scanRoot}`);
        console.log(`${LOG} updated ${snapshotPath}; cargo re-runs the route scan for this tree shape. A bare \`cargo\` build reaches the same result with ${RESCAN_ENV}=<any new value>.`);
    }
    return 0;
}

process.exitCode = main();
