// =============================================================================
// sync-provider-vault.ts — the VAULT pre-step for the cargo start verbs.
//
// WHY this exists (the `yarn start-rust` stale-endpoint bug): the Rust server does
// not read `config/vault/src/**`. It loads ONE derived file,
//   <repo>/config/vault/generated/agentic-provider.json
// (`packages/typecrate/provider/src/serve.rs:192-195` `DEFAULT_VAULT_ARTIFACT`), and
// it loads it exactly once per process (`serve.rs:415-436` `OnceLock`). That file is
// written only by `config/vault/scripts/export-vault.ts`, and until this pre-step
// NOTHING in the repository ran it automatically — so editing a vault module (e.g.
// retiring an endpoint in `config/vault/src/services/basalt.ts`) left a fresh
// `yarn start-rust` serving the bytes of the previous export. It was not a cache: it
// was a stale derived artifact, and it is fixed the same way every other derived
// input in this repo is fixed — regenerate it as a pre-step of starting.
//
// Two precedents this file copies:
//   * `scripts/discover-services.ts` — the sibling pre-step already wired into these
//     verbs (write-if-changed, `[<pkg>:<verb>]` log prefix, one focused script per
//     derived input, chain shape `… && cargo …`);
//   * `scripts/start-service.ts:4-7` — "ignored build output, never a source of
//     truth, so a fresh checkout cannot silently serve a missing or wrong-base page".
//     That is exactly this gap, one layer down.
//
// WHAT it runs: `config/vault/scripts/export-vault.ts --consumer agentic-provider`,
// i.e. the same command the Rust side prints in every refusal (`serve.rs:200-201`
// `REGENERATION_COMMAND`, reproduced verbatim below as the exported
// `REGENERATION_COMMAND`, with the scope itself pinned as `VAULT_CONSUMER`).
// Two flags are deliberately NOT passed:
//   * `--check`  — on a fresh clone `generated/` does not exist, `--check` reports
//     `missing` and exits 1 (export-vault.ts:452-456), so a start step must never use
//     it; the plain export CREATES the file instead of refusing;
//   * `--public-only` — it strips every `secret: true` slot entirely
//     (export-vault.ts:304-317) and writes to the SAME default path, which would hand
//     the server a vault with no credentials.
// The export is pure: no network, no `process.env`, no clock
// (config/vault/src/vault/snapshot.ts:6-8,140-142, enforced by purity.test.ts:48-60),
// and it is write-if-changed (export-vault.ts:445-450), so running it on every start
// costs one child process and touches the artifact's mtime only when the bytes really
// changed.
//
// -----------------------------------------------------------------------------
// FAILURE SEMANTICS (deliberate, and the whole point of the file)
// -----------------------------------------------------------------------------
// 0 - @config/vault is ABSENT (a standalone/embedded-repo checkout of this package,
//     which has no `config/` at all and no vault toolchain - see
//     `.github/workflows/ci.yml:16-27,87-89`): one line, no throw, cargo proceeds.
//     Such a checkout could not serve vault-backed routes anyway, and the server's own
//     load refusal (`serve.rs:415-436`) already prints the regeneration command, so a
//     hard failure here would only break cargo-only CI for no benefit.
//     0 is also the code for a successful `written` / `unchanged`.
// 1 - @config/vault is PRESENT and the export did not succeed: the artifact may be
//     stale, and starting a server on a known-stale endpoint pool is the bug this
//     pre-step exists to prevent, so the pre-step fails and the `&&` chain stops cargo
//     BEFORE the server can bind a port.
//
// The tree is never touched on the skip path; on the run path the only file written is
// the git-ignored artifact (`config/vault/.gitignore:8`), by the exporter itself.
//
// Flags: this step reads no flags of its own. The only argv it ever looks at is the
// `--script <file>` spelling of the ENTRY DETECTOR below (`isScriptToken`, ~line 380),
// which inspects argv without consuming anything. `start`/`run-native` end in a bare
// `--`, and npm appends forwarded CLI flags after the whole chain (package.json
// `#passthrough`), so they land on the cargo bin and never on this script.
// =============================================================================

import { spawnSync } from 'node:child_process';
import { existsSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
/** The npm package == the Cargo workspace root, like `discover-services.ts:51`. */
const packageDir = resolve(here, '..');
const LOG = '[typecrate-services:vault]';

// ----------------------------------------------------------------- constants
/**
 * The one scope this package's server consumes. It MUST stay the string the Rust
 * crate asks for by default (`serve.rs:200-201` `REGENERATION_COMMAND`); the export
 * script has no default and rejects an unknown id (export-vault.ts:226,285), so this
 * value is also the artifact file name (`generated/<consumer>.json`).
 */
export const VAULT_CONSUMER = 'agentic-provider';
/** The command the Rust refusal prints, reused here so the fix is spelled identically. */
export const REGENERATION_COMMAND = 'yarn workspace @config/vault export -- --consumer agentic-provider';
/**
 * `DEFAULT_ROOT_ANCESTOR_STEPS`, same as `discover-services.ts:61`: this package sits
 * at `<repo>/packages/<scope>/<name>`.
 *
 * NOTE on the route env override: `TYPECRATE_SERVICES_ROUTE_ROOT` moves discover's
 * SCAN root only — `discover-services.ts:114-126` keeps `repoRoot` at the plain
 * ancestor walk and applies the override to `scanRoot`. This step follows that
 * precedent exactly: the vault lives in the repository this package was published
 * inside, not wherever routes happen to be scanned from.
 */
const ROOT_ANCESTOR_STEPS = 3;

/** Where the three vault inputs live, relative to the repository root. */
export type VaultPaths = {
    /** `config/vault` — its `package.json` is the "the vault toolchain exists" probe. */
    vaultPackageDir: string;
    /** The derived file the Rust server loads, `generated/<consumer>.json`. */
    artifact: string;
};

/** Path assembly only — no fs access, so the test can assert it for any root. */
export const resolveVaultPaths = (repoRoot: string): VaultPaths => ({
    vaultPackageDir: join(repoRoot, 'config', 'vault'),
    artifact: join(repoRoot, 'config', 'vault', 'generated', `${VAULT_CONSUMER}.json`)
});

// --------------------------------------------------------------------- utils
/** Error → one line of text, so a failure never prints a bare [object Object]. */
const describe = (error: unknown): string => (error instanceof Error ? error.message : String(error));

/** Repository root: package dir + `ROOT_ANCESTOR_STEPS` levels up (mirrors discover). */
function ancestorDir(start: string, steps: number): string {
    let dir = start;
    for (let i = 0; i < steps; i += 1) dir = dirname(dir);
    return dir;
}

/**
 * vite-node's own CLI, found by walking up from the vault package dir: this repo
 * hoists it to `<root>/node_modules/vite-node` (root `AGENTS.md` "Runtime / runner
 * policy" — `vite-node` is root-hoisted and resolves from any package directory), and
 * a nested install is covered by the same walk. Copied from
 * `packages/typecrate/nexus/scripts/start-daytona.ts:163-175`.
 *
 * TOTAL-MISS FALLBACK — the honest description of what `'vite-node'` then does: it
 * becomes `args[0]` of `spawnSync(process.execPath, args)`, and node treats that first
 * argument as a MODULE PATH resolved against the child's cwd, NOT as a command looked
 * up on PATH (only the program itself is PATH-resolved, and that is
 * `process.execPath`). So the child dies on "Cannot find module 'vite-node'" and this
 * step FAILS CLOSED — exit 1, with node's own stderr relayed and the reason line
 * "exporter exited 1 with no summary line". That is the right outcome: the fallback is
 * only reachable when `config/vault` really is present but no vite-node is installed
 * within 8 levels (a monorepo whose dependencies were never installed), and starting
 * cargo on an artifact we could not regenerate is the bug being prevented. It is NOT
 * reached in a cargo-only checkout, which takes the absent-vault skip before anything
 * is spawned.
 */
const findViteNodeCli = (start: string): string => {
    for (let dir = start, depth = 0; depth < 8; depth += 1) {
        const candidate = join(dir, 'node_modules', 'vite-node', 'dist', 'cli.mjs');
        if (existsSync(candidate)) return candidate;
        const parent = dirname(dir);
        if (parent === dir) break;
        dir = parent;
    }
    return 'vite-node';
};

/**
 * The child script: `config/vault/scripts/export-vault.ts`. It is probed for
 * separately from `package.json` so a half-copied checkout takes the skip branch
 * instead of failing cargo on a runner error.
 */
export const vaultExportScript = (repoRoot: string): string => join(repoRoot, 'config', 'vault', 'scripts', 'export-vault.ts');

// ----------------------------------------------------------------------- deps
/** Result of one child run, reduced to what the decision needs. */
export type ChildRun = {
    /** 1 when the program could not be spawned or was killed by a signal. */
    code: number;
    stdout: string;
    stderr: string;
};

/**
 * The spawn options, as DATA so the two Windows-critical settings are pinnable:
 *   · `shell: false` (explicit, not the default) — with `shell: true` node hands a
 *     JOINED STRING to `cmd.exe` on Windows, where `PATHEXT`/`cmd.exe` shim resolution
 *     and the quoting of an absolute script path containing spaces are exactly the
 *     things that break, and a metacharacter in any argument becomes shell syntax.
 *   · `encoding: 'utf8'` — `stdout`/`stderr` arrive as strings, which is what
 *     {@link parseSummary} needs (a `Buffer` would silently parse as no summary).
 * `cwd` is per-run, so it travels inside the same object.
 */
export type RunOptions = { cwd: string; encoding: 'utf8'; shell: false };

/** The options for one child run — exported so the test pins them, not just the argv. */
export const buildRunOptions = (cwd: string): RunOptions => ({ cwd, encoding: 'utf8', shell: false });

/**
 * The seams, injected exactly like `ExportDeps` in
 * `config/vault/scripts/export-vault.ts:391-428` and the `SpawnImpl` seam in
 * `start-daytona.ts:973-996`: a `vi.mock` of a builtin rebinds the binding for the
 * TEST FILE but leaves this module calling the real one, so an explicit seam is the
 * only way to pin the argv, the spawn options, and the exit-code mapping without
 * spawning a second vault export (or a cargo build) per test.
 */
export type SyncDeps = {
    /** Real-file probe: a directory or a dangling symlink is NOT "the vault exists". */
    isFile: (path: string) => boolean;
    run: (command: string, args: readonly string[], options: RunOptions) => ChildRun;
    /** stdout — the one status line of a successful/skipped run. */
    log: (line: string) => void;
    /** stderr — the refusal line, plus the exporter's own diagnostics on a failure. */
    error: (line: string) => void;
};

export const createDefaultDeps = (): SyncDeps => ({
    isFile: (path) => {
        try {
            return statSync(path).isFile();
        } catch {
            // Anything the fs cannot describe (missing, unreadable, dangling link) is
            // "absent" — the soft branch.
            return false;
        }
    },
    run: (command, args, options) => {
        let result;
        try {
            // The options object is forwarded UNTOUCHED (`shell: false`,
            // `encoding: 'utf8'`), which is what `buildRunOptions` exists to pin.
            result = spawnSync(command, [...args], options);
        } catch (cause) {
            return { code: 1, stdout: '', stderr: describe(cause) };
        }
        // spawnSync reports "program not found" as `error` with `status: null`; a
        // signal kill is `status: null` with no error. Neither is a success.
        if (result.error) return { code: 1, stdout: '', stderr: describe(result.error) };
        return { code: result.status ?? 1, stdout: result.stdout ?? '', stderr: result.stderr ?? '' };
    },
    log: (line) => {
        console.log(line);
    },
    error: (line) => {
        console.error(line);
    }
});

// ------------------------------------------------------------------- summary
/** The fields this step reads out of the exporter's summary line. */
export type ExportSummary = { action: string; bytes?: number; out?: string };

/**
 * The exporter's contract (`export-vault.ts:190-196,459-491`): the LAST stdout line is
 * exactly one single-line JSON summary whose keys are never conditional. `--stdout`
 * can put a payload line FIRST, but nothing may follow the summary, so reading the
 * last non-empty line is the documented access path — not a search for "some JSON".
 * Anything else is `undefined`, which the verdict treats as an unknown state.
 */
export const parseSummary = (stdout: string): ExportSummary | undefined => {
    const lines = stdout.split(/\r?\n/);
    for (let index = lines.length - 1; index >= 0; index -= 1) {
        const line = lines[index]!.trim();
        if (line === '') continue;
        if (!line.startsWith('{')) return undefined;
        try {
            const parsed = JSON.parse(line) as { action?: unknown; bytes?: unknown; out?: unknown };
            return typeof parsed.action === 'string' ? { action: parsed.action, bytes: typeof parsed.bytes === 'number' ? parsed.bytes : undefined, out: typeof parsed.out === 'string' ? parsed.out : undefined } : undefined;
        } catch {
            return undefined;
        }
    }
    return undefined;
};

// ------------------------------------------------------------------- verdict
/**
 * The decision as DATA, with every message pre-rendered: no fs, no child, no
 * `process`, so the whole contract — exit codes, the exact wording, and which stream
 * each line belongs to — is pinned by the colocated test.
 */
export type SyncVerdict =
    | { kind: 'synced'; exitCode: 0; action: string; line: string }
    | { kind: 'failed'; exitCode: 1; line: string; relay: string[] };

/** The suffix every success line carries, so the reader knows what happens next. */
const READ_NOTE = 'the Rust process loads it once, on first use';

export const decideSync = (input: { artifact: string; child: ChildRun; summary: ExportSummary | undefined }): SyncVerdict => {
    const { artifact, child, summary } = input;
    const bytes = summary?.bytes === undefined ? '' : ` (${summary.bytes} bytes)`;
    const target = summary?.out || artifact;

    if (child.code === 0 && summary) {
        // `written` created or replaced the artifact; `unchanged` is the steady-state
        // answer and proves the write-if-changed mtime contract held.
        if (summary.action === 'written' || summary.action === 'unchanged') return { kind: 'synced', exitCode: 0, action: summary.action, line: `${LOG} artifact ${summary.action}: ${target}${bytes} — ${READ_NOTE}` };
        // `missing`/`drift`/`ok` only appear from a --check run, and any other name is
        // a vocabulary change on the exporter's side. Neither is a failure (the child
        // exited 0), so the action is surfaced verbatim rather than renamed.
        return { kind: 'synced', exitCode: 0, action: summary.action, line: `${LOG} artifact action=${summary.action}: ${target}${bytes} — ${READ_NOTE}` };
    }

    // Present vault, unusable outcome. The reason comes first, then the one command that
    // fixes it. `relay` carries the child's own stderr lines, which the caller prints
    // above this line ONLY on this failure path — `spawnSync` runs with its default
    // `stdio: 'pipe'`, so those lines reach us inside `child.stderr` and are discarded
    // unless a verdict says to print them (see `syncProviderVault`).
    const reason = summary
        ? `exporter exited ${child.code} reporting action=${summary.action}`
        : child.code === 0
          ? 'exporter exited 0 but its last stdout line is not the documented JSON summary'
          : `exporter exited ${child.code} with no summary line`;
    return {
        kind: 'failed',
        exitCode: 1,
        line: `${LOG} ✗ ${reason} — refusing to start cargo on a possibly stale artifact; regenerate with \`${REGENERATION_COMMAND}\``,
        relay: child.stderr.split(/\r?\n/).filter((line) => line.trim() !== '')
    };
};

/** The skip line, for a checkout that has no vault toolchain to run. The manifest path
 *  is joined (not concatenated) so the path it prints matches the platform's spelling. */
export const skipLine = (repoRoot: string, vaultPackageDir: string): string =>
    `${LOG} no config/vault toolchain under ${repoRoot} (${join(vaultPackageDir, 'package.json')} is absent) — standalone/cargo-only checkout: nothing to sync, cargo proceeds (the server prints \`${REGENERATION_COMMAND}\` itself if the artifact is missing)`;

// ----------------------------------------------------------------------- run
/** The child call: the node already running THIS file execs vite-node's CLI, which
 *  then loads the exporter — the same shape `scripts/start-service.ts` and
 *  `packages/typecrate/nexus/scripts/start-daytona.ts:516-540` use. Raw `node` on a
 *  repo `.ts` is forbidden (root `AGENTS.md`), and this Node build cannot load one at
 *  all (`process.features.typescript === false`), but `node <vite-node>/dist/cli.mjs`
 *  IS vite-node — exactly what the `node_modules/.bin/vite-node` shim execs. Passing
 *  the resolved CLI path (rather than the bare `vite-node` name) also sidesteps the
 *  PATH problem entirely: yarn only puts `node_modules/.bin` on PATH inside a
 *  lifecycle, and this step is spawned from inside one. */
export const buildInvocation = (repoRoot: string): { command: string; args: string[]; cwd: string } => ({
    // args[0] is vite-node's CLI, args[1] the exporter, then its required scope.
    command: process.execPath,
    args: [findViteNodeCli(join(repoRoot, 'config', 'vault')), vaultExportScript(repoRoot), '--consumer', VAULT_CONSUMER],
    // Same cwd `yarn workspace @config/vault export` produces. It is cosmetic here —
    // the exporter resolves every path from its own module, never from cwd
    // (export-vault.ts:87-144) — but agreeing with the documented verb keeps the child
    // comparable with a hand run.
    cwd: join(repoRoot, 'config', 'vault')
});

/**
 * The whole pre-step, as a function of (repo root, deps). Returns the exit code;
 * `process.exit` is never called, so importing this file from a test runs nothing.
 */
export const syncProviderVault = (repoRoot: string, deps: SyncDeps = createDefaultDeps()): number => {
    const paths = resolveVaultPaths(repoRoot);

    // Soft no-op: the vault package manifest is the probe for "this checkout has the
    // JS vault toolchain". `packages/typecrate/{services,provider}` are also published
    // as standalone repos whose checkouts have no `config/` directory at all
    // (.github/workflows/ci.yml:16-27,87-89) — there, cargo must be untouched.
    if (!deps.isFile(join(paths.vaultPackageDir, 'package.json'))) {
        deps.log(skipLine(repoRoot, paths.vaultPackageDir));
        return 0;
    }

    // Present manifest but no exporter script: fail closed. The vault exists, so an
    // artifact may exist, and "I could not verify it" is exactly the state the user
    // reported — booting on it is the bug.
    const script = vaultExportScript(repoRoot);
    if (!deps.isFile(script)) {
        deps.error(`${LOG} ✗ ${script} is missing although ${join(paths.vaultPackageDir, 'package.json')} is present — refusing to start cargo on a possibly stale artifact; regenerate with \`${REGENERATION_COMMAND}\``);
        return 1;
    }

    const invocation = buildInvocation(repoRoot);
    const child = deps.run(invocation.command, invocation.args, buildRunOptions(invocation.cwd));
    const verdict = decideSync({ artifact: paths.artifact, child, summary: parseSummary(child.stdout) });

    // SUCCESS prints exactly one line: the child's stderr (including the exporter's own
    // `[config-vault:export] wrote …` diagnostic) is NOT relayed here. `spawnSync` runs
    // with its default `stdio: 'pipe'` — nothing is inherited — so those lines exist only
    // inside `child.stderr`, and this branch simply does not print them: relaying them
    // would repeat the exporter's write report on every single start. On a FAILURE the
    // child's stderr IS relayed, because it holds the reason and the summary line alone
    // would not say what to fix.
    if (verdict.kind === 'failed') {
        for (const line of verdict.relay) deps.error(line);
        deps.error(verdict.line);
    } else {
        deps.log(verdict.line);
    }
    return verdict.exitCode;
};

/**
 * The repository root this step syncs: the package dir + 3 ancestors, resolved from
 * THIS module (never cwd), exactly like `discover-services.ts:125`.
 */
export const resolveRepoRoot = (): string => ancestorDir(packageDir, ROOT_ANCESTOR_STEPS);

// ══════════════════════════════════════════════════════════════════════════════
//  ENTRY POINT — importing this module must spawn and print nothing
// ══════════════════════════════════════════════════════════════════════════════
//
//  The three rules every script in this repo uses (config/vault/scripts/
//  export-vault.ts:587-665, packages/typecrate/provider/scripts/entry-guard.ts:13-24,
//  packages/typecrate/nexus/scripts/start-daytona.ts:1043-1129):
//    1. argv's SCRIPT POSITIONAL is this file — kept for a runner that leaves the file
//       on the command line;
//    2. the npm/yarn lifecycle line names it — this is the path `yarn start` actually
//       takes, because `npm_lifecycle_script` holds the whole wired verb:
//       "vite-node scripts/discover-services.ts && vite-node scripts/sync-provider-vault.ts && cargo run --bin typecrate-services-server --locked --";
//    3. vite-node evaluated THIS file as its entry — a plain `vite-node <file>` rewrites
//       `process.argv` and DROPS the file, so only the module-runner stack can see that
//       entry, and the stack has to be captured during module evaluation.
//  `VITEST` is checked first and wins, so the colocated test can import `main` and
//  every pure helper without spawning a single child process.

/** Extension-bearing token that identifies THIS file as the runnable script. */
const ENTRY_SCRIPT_PATTERN = /(?:^|[/\\])sync-provider-vault\.[cm]?[jt]sx?$/;
/** vite/vite-node append module ids and query strings; strip them before matching. */
const stripModuleQuery = (token: string): string => String(token).split('?')[0].split('#')[0];
const isScriptToken = (token: unknown): boolean => ENTRY_SCRIPT_PATTERN.test(stripModuleQuery(String(token)));

export type EntryPointSignals = {
    argv?: readonly string[];
    env?: Readonly<Record<string, string | undefined>>;
    modulePath?: string;
    /** Only meaningful while this module is still being evaluated (rule 3). */
    stack?: string;
    /** true under vitest, where nothing may run. */
    vitest?: boolean;
};

/** Rule 1: the script positional is this file (`vite-node … sync-provider-vault.ts`). */
const namesScriptPositional = (argv: readonly string[] | undefined): boolean => {
    const tokens = argv ?? [];
    if (isScriptToken(tokens[1])) return true;
    const flag = tokens.indexOf('--script');
    return flag !== -1 && isScriptToken(tokens[flag + 1]);
};

/** Rule 2: the lifecycle line names it — the verb that wires this step in. */
const namesLifecycleEntryPoint = (env: Readonly<Record<string, string | undefined>> | undefined): boolean =>
    String(env?.npm_lifecycle_script ?? '')
        .split(/\s+/)
        .some(isScriptToken);

/** Rule 3: vite-node evaluated THIS file as its entry — only runner frames above it. */
const isRunnerEntryPoint = (stack: string | undefined, modulePath: string | undefined): boolean => {
    if (!stack || !modulePath) return false;
    const frames = stack.split('\n').map((frame) => frame.trim().replace('file://', ''));
    if (!frames.some((frame) => frame.includes('ViteNodeRunner'))) return false;
    const own = frames.reduce((found, frame, index) => (frame.includes(modulePath) ? index : found), -1);
    if (own < 0) return false;
    const callers = frames.slice(own + 1);
    const executeFile = callers.findIndex((frame) => frame.includes('ViteNodeRunner.executeFile'));
    const upTo = executeFile === -1 ? callers.length : executeFile;
    return callers.slice(0, upTo).every((frame) => frame.includes('ViteNodeRunner'));
};

/** True when — and only when — this module is the process entry point. */
export const isCliEntryPoint = (signals: EntryPointSignals): boolean => {
    if (signals.vitest) return false;
    if (namesScriptPositional(signals.argv)) return true;
    if (namesLifecycleEntryPoint(signals.env)) return true;
    return isRunnerEntryPoint(signals.stack, signals.modulePath);
};

/** This file's own path (rule 3 needs to identify its own stack frames). */
const MODULE_FILE_PATH = (() => {
    try {
        const url = import.meta.url;
        if (typeof url === 'string' && url.startsWith('file://')) return fileURLToPath(url);
    } catch {
        /* vitest may hand a non-file import.meta.url — fall through to the pin */
    }
    return join(packageDir, 'scripts', 'sync-provider-vault.ts');
})();

/** Parses nothing, validates nothing: this step has no flags (see the header). */
export const main = (deps: SyncDeps = createDefaultDeps()): number => syncProviderVault(resolveRepoRoot(), deps);

// The stack must be captured during module evaluation; inside main() the evidence for
// rule 3 is already gone.
if (isCliEntryPoint({ argv: process.argv, env: process.env, modulePath: MODULE_FILE_PATH, stack: new Error().stack, vitest: Boolean(process.env.VITEST) })) {
    process.exitCode = main();
}
