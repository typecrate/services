// ─────────────────────────────────────────────────────────────────────────────
//  sync-provider-vault.test.ts — the vault pre-step's DECISION LOGIC, with no child
//  process and no cargo.
//
//  Why the seam: `syncProviderVault(root, deps)` takes its fs probe and its child
//  runner as parameters (same reasoning as `ExportDeps` in
//  config/vault/scripts/export-vault.ts:391-428 and the `SpawnImpl` seam in
//  packages/typecrate/nexus/scripts/start-daytona.ts:973-996 — a `vi.mock` of a
//  builtin rebinds the binding for the TEST FILE and leaves the module under test
//  calling the real one). So every branch below runs the real control flow against a
//  scratch root under repo-root `Temp/typecrate-services-vault/` (root AGENTS.md
//  scratch policy) and asserts EXACT strings, exact exit codes, and exact spawn calls.
//
//  What is pinned, and why each one matters:
//    · paths/pins  — the artifact path is the ONE file the Rust server also loads
//                    (packages/typecrate/provider/src/serve.rs:192-195) and the
//                    consumer id is the string in serve.rs:200-201, so a rename on
//                    either side is a real regression, not a cosmetic one;
//    · invocation  — the child is `process.execPath` + vite-node's hoisted CLI + the
//                    exporter + `--consumer`, with NO `--check` (a fresh clone would
//                    exit 1 on `missing`) and NO `--public-only` (it strips secrets);
//    · run options — `shell:false` and `encoding:'utf8'` are pinned at the seam AND
//                    exercised against one real trivial child, so a future `shell:true`
//                    (cmd.exe/PATHEXT exposure on Windows, and a Buffer instead of a
//                    string for `parseSummary`) fails here instead of in a start;
//    · summary     — the exporter guarantees the summary is the LAST stdout line
//                    (export-vault.ts:190-196) and `--stdout` puts its payload line
//                    FIRST, so "last, not first" is asserted rather than assumed;
//    · decide      — exit 0 for `written`/`unchanged`, exit 1 for any failed export,
//                    and a refusal line that always names the regeneration command;
//    · sync       — absent vault → one line, exit 0, NOTHING spawned (a cargo-only
//                    checkout of the embedded repo must be unaffected); present vault +
//                    failed export → exit 1, so the `&&` chain never reaches cargo;
//    · quietness   — a SUCCESSFUL run relays NOTHING of the child's stderr (stdout is
//                    piped, and `start-rust` must stay a one-line-per-start affair);
//    · package.json — the wiring contract, asserted as CONTRACT rather than whole-string
//                    equality for verbs this step does not own (`check:ui` etc. are the
//                    user's in-flight rename surface): `start`/`run-native` chain the
//                    pre-step EXACTLY once and strictly before cargo, the bare `--`
//                    stays the last token, the two verbs agree, the non-serving verbs
//                    carry no vault step, and `test:scripts` exists so this file is
//                    actually executed (that verb is the reason these tests run at all);
//    · entry      — `isCliEntryPoint`'s rules, i.e. the guarantee that importing this
//                    module (this very file) starts nothing.
// ─────────────────────────────────────────────────────────────────────────────

import { existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { isAbsolute, join } from 'node:path';
import { afterAll, describe, expect, it } from 'vitest';

import {
    buildInvocation,
    buildRunOptions,
    createDefaultDeps,
    decideSync,
    isCliEntryPoint,
    parseSummary,
    REGENERATION_COMMAND,
    resolveRepoRoot,
    resolveVaultPaths,
    syncProviderVault,
    VAULT_CONSUMER,
    vaultExportScript,
    type ChildRun,
    type RunOptions,
    type SyncDeps
} from './sync-provider-vault';

// ── Scratch: repo-root Temp/typecrate-services-vault/, per root AGENTS.md ──────
// `resolveRepoRoot()` is the step's own pin, so the scratch root needs no cwd guess.
// PRECONDITION, created rather than assumed: `mkdtemp` creates ONLY its last segment,
// so without the recursive mkdir a clean clone dies at collection with ENOENT (root
// `Temp/` is git-ignored and disposable by policy).
const REPO_ROOT = resolveRepoRoot();
const SCRATCH_ROOT = join(REPO_ROOT, 'Temp', 'typecrate-services-vault');
mkdirSync(SCRATCH_ROOT, { recursive: true });
const SCRATCH = mkdtempSync(join(SCRATCH_ROOT, 'test-'));

afterAll(() => {
    rmSync(SCRATCH, { recursive: true, force: true });
});

/** The hoisted vite-node CLI the child is really execed with (root-hoisted, AGENTS.md). */
const VITE_NODE_CLI = join(REPO_ROOT, 'node_modules', 'vite-node', 'dist', 'cli.mjs');

/** A fresh fake repository root per test, so no two tests share a generated/ tree. */
const freshRoot = (name: string): string => {
    const root = join(SCRATCH, name);
    mkdirSync(join(root, 'packages', 'typecrate', 'services', 'scripts'), { recursive: true });
    return root;
};

/** Dep recorder over an explicit file set: no fs reads at all, and every call — the
 *  argv AND the spawn options — is visible, so `shell:false`/`encoding:'utf8'` have
 *  real coverage and a future `shell:true` fails a test instead of passing one. */
const makeDeps = (files: readonly string[], child: ChildRun = { code: 0, stdout: '', stderr: '' }) => {
    const log: string[] = [];
    const error: string[] = [];
    const runs: Array<{ command: string; args: readonly string[]; options: RunOptions }> = [];
    const set = new Set(files);

    const deps: SyncDeps = {
        isFile: (path) => set.has(path),
        run: (command, args, options) => {
            runs.push({ command, args, options });
            return child;
        },
        log: (line) => {
            log.push(line);
        },
        error: (line) => {
            error.push(line);
        }
    };

    return { deps, log, error, runs };
};

/** The exact JSON the exporter prints as its LAST stdout line (measured output). */
const summaryLine = (action: string, out: string, bytes: number): string =>
    `{"consumer":"agentic-provider","publicOnly":false,"out":"${out}","action":"${action}","schemaVersion":1,"vendors":11,"slots":20,"secretSlots":9,"entries":42,"bytes":${bytes},"diskBytes":${action === 'written' ? 'null' : bytes}}`;

// The message wording, spelled out here (never rebuilt from the module's own helpers)
// so a reworded line has to be a deliberate edit to this file. `skipLineFor` restates
// the ABSENT-vault line independently: asserting `skipLine()` against itself would pin
// nothing, so every skip assertion below compares against THIS literal.
const LOG = '[typecrate-services:vault]';
const REGEN = '`yarn workspace @config/vault export -- --consumer agentic-provider`';
const REFUSAL = `${LOG} ✗`;
const skipLineFor = (root: string): string =>
    `${LOG} no config/vault toolchain under ${root} (${join(root, 'config', 'vault', 'package.json')} is absent) — standalone/cargo-only checkout: nothing to sync, cargo proceeds (the server prints ${REGEN} itself if the artifact is missing)`;

describe('pins — the artifact path and consumer the Rust server reads', () => {
    it('resolves the derived artifact the provider loads, and only that one', () => {
        expect(VAULT_CONSUMER).toBe('agentic-provider');
        expect(resolveVaultPaths('/repo')).toEqual({
            vaultPackageDir: join('/repo', 'config', 'vault'),
            artifact: join('/repo', 'config', 'vault', 'generated', 'agentic-provider.json')
        });
        // Byte-identical to provider/src/serve.rs:200-201 REGENERATION_COMMAND.
        expect(REGENERATION_COMMAND).toBe('yarn workspace @config/vault export -- --consumer agentic-provider');
    });

    it('names the exporter script next to the vault manifest', () => {
        expect(vaultExportScript('/repo')).toBe(join('/repo', 'config', 'vault', 'scripts', 'export-vault.ts'));
    });

    it('resolves the repository root from the package location, not from cwd', () => {
        // The root is the one directory that holds BOTH vault inputs and the hoisted
        // runner the child execs — the three things this step actually needs.
        expect(isAbsolute(REPO_ROOT)).toBe(true);
        expect(existsSync(join(REPO_ROOT, 'config', 'vault', 'scripts', 'export-vault.ts'))).toBe(true);
        expect(existsSync(join(REPO_ROOT, 'packages', 'typecrate', 'services', 'scripts', 'sync-provider-vault.ts'))).toBe(true);
        expect(existsSync(VITE_NODE_CLI)).toBe(true);
    });
});

describe('buildInvocation / buildRunOptions — the child the pre-step spawns', () => {
    it('runs vite-node on the exporter with the required scope, and nothing else', () => {
        // args[1..] are the whole contract: the exporter, then its required scope. No
        // --check (a fresh clone reports `missing` and exits 1) and no --public-only
        // (it strips every secret slot into the SAME path, export-vault.ts:304-317,377).
        // args[0] is the PATH fallback here: `/repo` has no node_modules above it, and
        // node resolves that first argument as a MODULE PATH, so the child would die on
        // "Cannot find module" and the pre-step would fail closed (exit 1).
        expect(buildInvocation('/repo').args).toEqual(['vite-node', join('/repo', 'config', 'vault', 'scripts', 'export-vault.ts'), '--consumer', 'agentic-provider']);
        expect(buildInvocation('/repo').command).toBe(process.execPath);
        expect(buildInvocation('/repo').cwd).toBe(join('/repo', 'config', 'vault'));
    });

    it('prefers the hoisted vite-node CLI of a real checkout', () => {
        expect(buildInvocation(REPO_ROOT).args).toEqual([VITE_NODE_CLI, join(REPO_ROOT, 'config', 'vault', 'scripts', 'export-vault.ts'), '--consumer', 'agentic-provider']);
    });

    it('spawns WITHOUT a shell and decodes as utf8 — the two Windows-critical options', () => {
        // `shell:true` would join the argv into a cmd.exe command line (PATHEXT/shim
        // resolution, metacharacters becoming shell syntax) and `encoding:'utf8'` is
        // what makes `stdout` a string for parseSummary. Both are data, so both pin.
        expect(buildRunOptions('/somewhere')).toEqual({ cwd: '/somewhere', encoding: 'utf8', shell: false });
    });
});

describe('parseSummary — the exporter guarantees the summary is the LAST stdout line', () => {
    const OUT = join('/repo', 'config', 'vault', 'generated', 'agentic-provider.json');

    it('reads action/bytes/out out of the documented summary', () => {
        expect(parseSummary(`${summaryLine('written', OUT, 14779)}\n`)).toEqual({ action: 'written', bytes: 14779, out: OUT });
        expect(parseSummary(summaryLine('unchanged', OUT, 14779))).toEqual({ action: 'unchanged', bytes: 14779, out: OUT });
    });

    it('takes the summary, never the --stdout payload line that comes first', () => {
        // The payload is the vault document (schema_version/package/consumers/…), so a
        // "first JSON line" implementation would report no action at all here.
        const payload = '{"schema_version":1,"package":"@config/vault","consumers":["agentic-provider"]}';
        expect(parseSummary(`${payload}\n${summaryLine('unchanged', OUT, 14779)}\n`)).toEqual({ action: 'unchanged', bytes: 14779, out: OUT });
    });

    it('reports no summary for empty, non-JSON, or action-less output', () => {
        expect(parseSummary('')).toBeUndefined();
        expect(parseSummary('   \n\n')).toBeUndefined();
        expect(parseSummary('some other tool spoke\n')).toBeUndefined();
        expect(parseSummary('{"schema_version":1}\n')).toBeUndefined();
        expect(parseSummary('{not json}\n')).toBeUndefined();
    });
});

describe('decideSync — exit codes and the exact lines they print', () => {
    const ARTIFACT = join('/repo', 'config', 'vault', 'generated', 'agentic-provider.json');
    const base = { artifact: ARTIFACT };

    it('a written artifact is success and says so exactly once', () => {
        const summary = parseSummary(summaryLine('written', ARTIFACT, 14779));
        expect(decideSync({ ...base, child: { code: 0, stdout: `${summaryLine('written', ARTIFACT, 14779)}\n`, stderr: '' }, summary })).toEqual({
            kind: 'synced',
            exitCode: 0,
            action: 'written',
            line: `${LOG} artifact written: ${ARTIFACT} (14779 bytes) — the Rust process loads it once, on first use`
        });
    });

    it('an unchanged artifact is success — the steady-state answer, and the mtime proof', () => {
        const summary = parseSummary(summaryLine('unchanged', ARTIFACT, 14779));
        expect(decideSync({ ...base, child: { code: 0, stdout: '', stderr: '' }, summary })).toEqual({
            kind: 'synced',
            exitCode: 0,
            action: 'unchanged',
            line: `${LOG} artifact unchanged: ${ARTIFACT} (14779 bytes) — the Rust process loads it once, on first use`
        });
    });

    it('surfaces an unexpected action verbatim instead of renaming it', () => {
        // `ok`/`drift`/`missing` can only come from a --check run, and the child exited
        // 0, so the step reports what it saw rather than inventing `written`.
        expect(decideSync({ ...base, child: { code: 0, stdout: '', stderr: '' }, summary: { action: 'missing', bytes: undefined, out: undefined } })).toEqual({
            kind: 'synced',
            exitCode: 0,
            action: 'missing',
            line: `${LOG} artifact action=missing: ${ARTIFACT} — the Rust process loads it once, on first use`
        });
    });

    it('a non-zero export fails closed, relays the child stderr, and names the fix', () => {
        expect(
            decideSync({
                ...base,
                child: { code: 1, stdout: `${summaryLine('drift', ARTIFACT, 14779)}\n`, stderr: '[config-vault:export] ✗ boom\n' },
                summary: parseSummary(summaryLine('drift', ARTIFACT, 14779))
            })
        ).toEqual({
            kind: 'failed',
            exitCode: 1,
            line: `${REFUSAL} exporter exited 1 reporting action=drift — refusing to start cargo on a possibly stale artifact; regenerate with ${REGEN}`,
            relay: ['[config-vault:export] ✗ boom']
        });
    });

    it('a crash with no summary is still one line, and still exit 1', () => {
        expect(decideSync({ ...base, child: { code: 1, stdout: '', stderr: 'Error: Cannot find module' }, summary: undefined })).toEqual({
            kind: 'failed',
            exitCode: 1,
            line: `${REFUSAL} exporter exited 1 with no summary line — refusing to start cargo on a possibly stale artifact; regenerate with ${REGEN}`,
            relay: ['Error: Cannot find module']
        });
    });

    it('exit 0 with no summary is a failure: an unknown artifact state must not boot cargo', () => {
        expect(decideSync({ ...base, child: { code: 0, stdout: 'command not found\n', stderr: '' }, summary: undefined })).toEqual({
            kind: 'failed',
            exitCode: 1,
            line: `${REFUSAL} exporter exited 0 but its last stdout line is not the documented JSON summary — refusing to start cargo on a possibly stale artifact; regenerate with ${REGEN}`,
            relay: []
        });
    });
});

describe('syncProviderVault — the branch contract of the wired pre-step', () => {
    it('ABSENT vault: one line, exit 0, nothing spawned, nothing written', () => {
        const root = freshRoot('absent-vault');
        const probe = makeDeps([]);

        expect(syncProviderVault(root, probe.deps)).toBe(0);
        expect(probe.runs).toEqual([]);
        expect(probe.error).toEqual([]);
        // Compared against the test's OWN literal, never against `skipLine()`.
        expect(probe.log).toEqual([skipLineFor(root)]);
    });

    it('PRESENT vault, current tree: exit 0, exact spawn call, one stdout line, no stderr', () => {
        const root = freshRoot('present-unchanged');
        const paths = resolveVaultPaths(root);
        const probe = makeDeps([join(paths.vaultPackageDir, 'package.json'), vaultExportScript(root)], { code: 0, stdout: `${summaryLine('unchanged', paths.artifact, 14779)}\n`, stderr: '' });

        expect(syncProviderVault(root, probe.deps)).toBe(0);
        // The full child call, options included: `shell:false` + `encoding:'utf8'`.
        expect(probe.runs).toEqual([{ command: process.execPath, args: [VITE_NODE_CLI, vaultExportScript(root), '--consumer', 'agentic-provider'], options: { cwd: paths.vaultPackageDir, encoding: 'utf8', shell: false } }]);
        expect(probe.error).toEqual([]);
        expect(probe.log).toEqual([`${LOG} artifact unchanged: ${paths.artifact} (14779 bytes) — the Rust process loads it once, on first use`]);
    });

    it('PRESENT vault, first start on a fresh clone: the export CREATES the artifact (exit 0)', () => {
        const root = freshRoot('present-written');
        const paths = resolveVaultPaths(root);
        // A non-empty child stderr on success MUST stay invisible: stdout is piped, and
        // `start-rust` prints exactly one line whether or not the bytes changed.
        const probe = makeDeps([join(paths.vaultPackageDir, 'package.json'), vaultExportScript(root)], {
            code: 0,
            stdout: `${summaryLine('written', paths.artifact, 14779)}\n`,
            stderr: `[config-vault:export] wrote 11 vendor(s) / 20 slot(s) / 14779 bytes → ${paths.artifact} (new file)\n`
        });

        expect(syncProviderVault(root, probe.deps)).toBe(0);
        expect(probe.log).toEqual([`${LOG} artifact written: ${paths.artifact} (14779 bytes) — the Rust process loads it once, on first use`]);
        expect(probe.error).toEqual([]);
    });

    it('PRESENT vault, failed export: exit 1 so cargo never boots on a stale artifact', () => {
        const root = freshRoot('present-failed');
        const paths = resolveVaultPaths(root);
        const probe = makeDeps([join(paths.vaultPackageDir, 'package.json'), vaultExportScript(root)], { code: 1, stdout: `${summaryLine('drift', paths.artifact, 14779)}\n`, stderr: '[config-vault:export] ✗ I/O failure\n' });

        expect(syncProviderVault(root, probe.deps)).toBe(1);
        expect(probe.log).toEqual([]);
        expect(probe.error).toEqual(['[config-vault:export] ✗ I/O failure', `${REFUSAL} exporter exited 1 reporting action=drift — refusing to start cargo on a possibly stale artifact; regenerate with ${REGEN}`]);
    });

    it('a spawn that never starts (no runner resolvable) is exit 1, with the OS reason relayed', () => {
        const root = freshRoot('spawn-error');
        const paths = resolveVaultPaths(root);
        const probe = makeDeps([join(paths.vaultPackageDir, 'package.json'), vaultExportScript(root)], { code: 1, stdout: '', stderr: 'ENOENT: no such file or directory, spawnSync vite-node' });

        expect(syncProviderVault(root, probe.deps)).toBe(1);
        expect(probe.log).toEqual([]);
        expect(probe.error).toEqual(['ENOENT: no such file or directory, spawnSync vite-node', `${REFUSAL} exporter exited 1 with no summary line — refusing to start cargo on a possibly stale artifact; regenerate with ${REGEN}`]);
    });

    it('a half-copied checkout (manifest, no exporter) fails closed instead of skipping', () => {
        const root = freshRoot('no-exporter');
        const paths = resolveVaultPaths(root);
        const probe = makeDeps([join(paths.vaultPackageDir, 'package.json')]);

        expect(syncProviderVault(root, probe.deps)).toBe(1);
        expect(probe.runs).toEqual([]);
        expect(probe.log).toEqual([]);
        expect(probe.error).toEqual([`${REFUSAL} ${vaultExportScript(root)} is missing although ${join(paths.vaultPackageDir, 'package.json')} is present — refusing to start cargo on a possibly stale artifact; regenerate with ${REGEN}`]);
    });
});

describe('createDefaultDeps — the real probes, on the real fs, under Temp/', () => {
    it('reads a real file as present and a real directory as absent', () => {
        const deps = createDefaultDeps();
        const dir = join(SCRATCH, 'probe-dir');
        const file = join(dir, 'package.json');
        mkdirSync(dir, { recursive: true });
        writeFileSync(file, '{}\n');

        expect(deps.isFile(file)).toBe(true);
        // A directory named package.json is NOT "the vault exists" — it cannot be run.
        expect(deps.isFile(dir)).toBe(false);
        expect(deps.isFile(join(SCRATCH, 'never-created.json'))).toBe(false);
    });

    it('an empty fake root takes the soft skip with the REAL deps (AC3)', () => {
        const root = join(SCRATCH, 'real-deps-absent');
        mkdirSync(root, { recursive: true });
        const log: string[] = [];
        const deps: SyncDeps = { ...createDefaultDeps(), log: (line) => log.push(line) };

        expect(syncProviderVault(root, deps)).toBe(0);
        // The same literal as the fake-deps branch: the real probe reaches the same line.
        expect(log).toEqual([skipLineFor(root)]);
        // Nothing on the skip path may create the artifact directory.
        expect(existsSync(join(root, 'config'))).toBe(false);
    });

    it('the real run honours cwd, the utf8 decode, and the no-shell argv (measured)', () => {
        // One real child, node's own `-e`, so this pins what the DEFAULT deps actually
        // do with `buildRunOptions`. The payload deliberately carries `|` and `&&`
        // plus a non-ASCII byte: under `shell:true` a POSIX shell/cmd.exe would eat the
        // pipe, and without `encoding:'utf8'` stdout would be a Buffer, which fails the
        // `toEqual` on a plain string below (and parseSummary would never match).
        const dir = join(SCRATCH, 'probe-child');
        mkdirSync(dir, { recursive: true });
        // The child reports ITS OWN cwd, so the assertion proves the option was applied
        // rather than merely handed over. realpathSync: a symlinked checkout root would
        // otherwise make the child's cwd and the joined path differ.
        const child = createDefaultDeps().run(process.execPath, ['-e', "process.stdout.write(process.cwd() + '|é|' + (1+1))"], buildRunOptions(dir));

        expect(child).toEqual({ code: 0, stdout: `${realpathSync(dir)}|é|2`, stderr: '' });
    });

    it('a command that cannot be spawned resolves to code 1 with a stderr reason', () => {
        // The default deps must never surface a `null` status (a signal kill or a
        // missing program), because `decideSync` maps only numbers onto a verdict.
        const missing = createDefaultDeps().run('definitely-not-a-program-xyz', ['--version'], buildRunOptions(SCRATCH));

        expect(missing.code).toBe(1);
        expect(missing.stdout).toBe('');
        expect(missing.stderr.includes('definitely-not-a-program-xyz')).toBe(true);
    });
});

describe('the wired verbs in packages/typecrate/services/package.json', () => {
    const servicesPackage = JSON.parse(readFileSync(join(REPO_ROOT, 'packages', 'typecrate', 'services', 'package.json'), 'utf8')) as { scripts: Record<string, string> };
    const scripts = servicesPackage.scripts;
    const VAULT_PRE_STEP = 'vite-node scripts/sync-provider-vault.ts';

    // CONTRACT assertions, not whole-string equality: the cargo legs and the sibling
    // verbs (`check:ui` names scripts/check-playground.ts, which is mid-.mjs→.ts-rename
    // in the user's working tree) belong to other changes, and a vault test must not
    // fail because one of them moved. What IS this change's contract is asserted exactly.
    it('start and run-native chain the vault pre-step EXACTLY once, before cargo', () => {
        for (const verb of ['start', 'run-native']) {
            const script = scripts[verb];
            expect(script, verb).toBeDefined();
            // Exactly once: a duplicated pre-step would re-export (and re-print) twice.
            expect(script.split(VAULT_PRE_STEP).length - 1, verb).toBe(1);
            // Strictly before cargo starts, or the server could bind before the sync.
            expect(script.indexOf(VAULT_PRE_STEP), verb).toBeLessThan(script.indexOf('cargo '));
            expect(script.indexOf('cargo '), verb).toBeGreaterThan(-1);
        }
    });

    it('the bare `--` pass-through stays the LAST token of both start verbs', () => {
        // npm appends forwarded flags to the end of the script string (`#passthrough`),
        // so nothing may be appended after the separator.
        for (const verb of ['start', 'run-native']) {
            const tokens = scripts[verb].trim().split(/\s+/);
            expect(tokens.at(-1), verb).toBe('--');
        }
    });

    it('run-native stays an exact alias of start', () => {
        expect(scripts['run-native']).toBe(scripts.start);
    });

    it('the verbs that never boot a server do not run the pre-step', () => {
        for (const verb of ['discover', 'build', 'check', 'test', 'verify', 'check:ui', 'lint', 'lint:strict', 'fmt', 'fmt:check']) {
            expect(scripts[verb], verb).toBeDefined();
            expect(scripts[verb], verb).not.toContain('sync-provider-vault');
        }
    });

    it('test:scripts exists, so this file is executed by something', () => {
        // `test` is cargo, turbo visits only `test`, and the package's CI runs `npm test`
        // (.github/workflows/ci.yml:91-93); without this verb nothing executes these
        // tests. Same verb as packages/typecrate/provider/package.json:33 and
        // packages/typecrate/nexus/package.json:34 declare, and asserted as a contract
        // (a vitest run scoped to `scripts`) rather than a pinned spelling.
        expect(scripts['test:scripts']).toContain('vitest');
        expect(scripts['test:scripts']).toContain('scripts');
    });
});

describe('isCliEntryPoint — importing this module spawns nothing', () => {
    const MODULE = join(REPO_ROOT, 'packages', 'typecrate', 'services', 'scripts', 'sync-provider-vault.ts');

    it('vitest always wins, which is what keeps THIS file side-effect free', () => {
        expect(isCliEntryPoint({ vitest: true, argv: ['vite-node', MODULE], env: { npm_lifecycle_script: 'vite-node scripts/sync-provider-vault.ts' }, modulePath: MODULE, stack: `Error\n at ViteNodeRunner.executeFile (${MODULE})` })).toBe(false);
    });

    it('rule 1: the script positional names this file (module query strings included)', () => {
        expect(isCliEntryPoint({ argv: ['vite-node', MODULE, '--extra'], modulePath: MODULE })).toBe(true);
        expect(isCliEntryPoint({ argv: ['vite-node', `${MODULE}?direct`], modulePath: MODULE })).toBe(true);
        expect(isCliEntryPoint({ argv: ['vite-node', '--script', './scripts/sync-provider-vault.ts'], modulePath: MODULE })).toBe(true);
        expect(isCliEntryPoint({ argv: ['vite-node', join(REPO_ROOT, 'packages', 'typecrate', 'services', 'scripts', 'discover-services.ts')], modulePath: MODULE })).toBe(false);
    });

    it('rule 2: the wired lifecycle line names this file, and a sibling verb does not', () => {
        // A literal input here (not a read of package.json): the guard must recognise the
        // lifecycle line `start` produces, whatever else that verb later grows.
        expect(isCliEntryPoint({ argv: ['vite-node', 'x.ts'], env: { npm_lifecycle_script: 'vite-node scripts/discover-services.ts && vite-node scripts/sync-provider-vault.ts && cargo run --bin typecrate-services-server --locked --' } })).toBe(true);
        expect(isCliEntryPoint({ argv: ['vite-node', 'x.ts'], env: { npm_lifecycle_script: 'vite-node scripts/discover-services.ts && cargo build --release --locked' } })).toBe(false);
        expect(isCliEntryPoint({ argv: ['vite-node', 'x.ts'], env: { npm_lifecycle_script: 'vitest run' } })).toBe(false);
    });
});
