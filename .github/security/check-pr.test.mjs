import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, symlinkSync, unlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { afterEach, beforeEach, test } from 'node:test';
import { formatSummary, inspect } from './check-pr.mjs';

const policy = readFileSync(new URL('../CODEOWNERS', import.meta.url), 'utf8');
let directory;
let previous;
let base;

function git(...args) {
  return execFileSync('git', ['--literal-pathspecs', ...args], { stdio: ['ignore', 'pipe', 'pipe'] }).toString('utf8').trim();
}

function write(path, content) {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, content);
}

function commit() {
  git('add', '.');
  git('commit', '-qm', 'contributor change');
  return git('rev-parse', 'HEAD');
}

function check() {
  return inspect(base, commit(), policy);
}

beforeEach(() => {
  previous = process.cwd();
  directory = mkdtempSync(join(tmpdir(), 'surfpool-security-'));
  process.chdir(directory);
  git('init', '-q');
  git('config', 'user.name', 'Test');
  git('config', 'user.email', 'test@example.invalid');
  write('Cargo.toml', '[workspace]\nmembers = ["crates/core"]\nexclude = ["examples/*"]\n');
  write('Cargo.lock', 'version = 4\n');
  write('crates/core/Cargo.toml', '[package]\nname = "core"\nversion = "0.1.0"\n');
  write('examples/demo/Cargo.toml', '[package]\nname = "demo"\nversion = "0.1.0"\n');
  write('examples/demo/Cargo.lock', 'version = 4\n');
  write('crates/sdk-node/package.json', '{"name":"sdk"}\n');
  write('crates/sdk-node/package-lock.json', '{"lockfileVersion":3}\n');
  base = commit();
});

afterEach(() => {
  process.chdir(previous);
  rmSync(directory, { recursive: true, force: true });
});

test('ordinary documentation produces no configured indicators', () => {
  write('doc/guide.md', 'Run surfpool to start your local network.\n');
  const result = check();
  assert.equal(result.sensitive.size, 0);
  assert.deepEqual(result.findings, []);
  assert.match(formatSummary(result), /No configured sensitive-change indicators detected/);
});

test('sensitive surfaces identify code ownership without extra findings solely on path matches', () => {
  write('crates/core/src/rpc/new.rs', 'pub fn rpc_method() {}\n');
  write('.github/actions/example/action.yml', 'name: Example\n');
  write('crates/core/src/tests/assets/payload', Buffer.from([0, 1, 2]));
  const result = check();
  assert.equal(result.sensitive.size, 3);
  assert.deepEqual(result.findings, []);
  assert.match(formatSummary(result), /Potentially sensitive changes detected/);
});

test('lockfile-only transitive updates are reported', () => {
  write('Cargo.lock', 'version = 4\n# transitive update\n');
  assert.ok(check().findings.some(({ reason }) => reason.includes('owning manifest')));
});

test('a registered workspace member manifest satisfies the root lock', () => {
  write('Cargo.lock', 'version = 4\n# dependency update\n');
  write('crates/core/Cargo.toml', '[package]\nname = "core"\nversion = "0.2.0"\n');
  assert.deepEqual(check().findings, []);
});

test('an excluded example does not satisfy the workspace lock', () => {
  write('Cargo.lock', 'version = 4\n# update\n');
  write('examples/demo/Cargo.toml', '[package]\nname = "demo"\nversion = "0.2.0"\n');
  assert.ok(check().findings.length);
});

test('independent locks need their own manifests', () => {
  write('examples/demo/Cargo.lock', 'version = 4\n# update\n');
  write('Cargo.toml', '[workspace]\nmembers = ["crates/core"]\n# unrelated\n');
  write('crates/sdk-node/package-lock.json', '{"lockfileVersion":2}\n');
  assert.deepEqual(new Set(check().findings.map(({ path }) => path)), new Set([
    'examples/demo/Cargo.lock', 'crates/sdk-node/package-lock.json',
  ]));
});

test('payloads hidden in fixtures are still scanned', () => {
  const payload = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/'.repeat(2);
  write('crates/core/src/tests/assets/new.json', JSON.stringify({ data: payload }) + '\n');
  assert.ok(check().findings.some(({ reason }) => reason.includes('high-entropy')));
});

test('existing fixture payloads do not block unrelated additions', () => {
  const payload = JSON.stringify('a'.repeat(300)) + '\n';
  write('crates/core/src/tests/assets/example.json', payload);
  base = commit();
  write('crates/core/src/tests/assets/example.json', payload + '{}\n');
  assert.deepEqual(check().findings, []);
});

test('unknown binaries and new network surfaces need ownership', () => {
  write('doc/innocent.txt', Buffer.from([0, 1, 2]));
  write('new_client.rs', 'fn client() { reqwest::get(url); }\n');
  const result = check();
  assert.deepEqual(new Set(result.sensitive.keys()), new Set(['doc/innocent.txt', 'new_client.rs']));
  assert.equal(result.findings.length, 2);
});

test('renaming away from a sensitive path retains an ownership indicator', () => {
  write('crates/core/src/rpc/old.rs', 'pub fn api() {}\n');
  base = commit();
  renameSync('crates/core/src/rpc/old.rs', 'ordinary.rs');
  assert.ok(check().sensitive.has('crates/core/src/rpc/old.rs'));
});

test('escape encoding and dynamic execution are reported', () => {
  write('scripts/install.js', 'const x = "' + '\\x41'.repeat(10) + '";\n' + 'ev' + 'al(x);\n');
  const { findings } = check();
  assert.ok(findings.some(({ reason }) => reason.includes('escapes')));
  assert.ok(findings.some(({ reason }) => reason.includes('dynamic execution')));
});

test('pathspec magic cannot hide payloads', () => {
  write(':(exclude)innocent.txt', 'a'.repeat(300) + '\n');
  assert.ok(check().findings.length);
});

test('new executables require ownership', () => {
  write('innocent', '#!/bin/sh\necho hello\n');
  chmodSync('innocent', 0o755);
  assert.ok(check().findings.length);
});

test('unknown binary deletions require owner review', () => {
  write('innocent.txt', Buffer.from([0, 1, 2]));
  base = commit();
  unlinkSync('innocent.txt');
  const result = check();
  assert.ok(result.sensitive.has('innocent.txt'));
  assert.ok(result.findings.length);
});

test('unknown symlinks are not followed', () => {
  symlinkSync('/etc/passwd', 'innocent.txt');
  const { findings } = check();
  assert.equal(findings.length, 1);
  assert.match(findings[0].reason, /symlink/);
});


test('a Rust startup fetch is highlighted and remains advisory', () => {
  const path = 'crates/cli/src/main.rs';
  write(path, 'async fn main() { reqwest::get("https://example.invalid/startup").await; }\n');
  const head = commit();
  const result = inspect(base, head, policy);
  assert.ok(result.sensitive.get(path).has('sensitive API primitive'));
  const summary = join(directory, 'summary.md');
  const run = spawnSync(process.execPath, [fileURLToPath(new URL('./check-pr.mjs', import.meta.url)), base, head], {
    encoding: 'utf8', env: { ...process.env, GITHUB_STEP_SUMMARY: summary },
  });
  assert.equal(run.status, 0, run.stdout + run.stderr);
  assert.match(readFileSync(summary, 'utf8'), /sensitive API primitive/);
});

test('lockfile drift and encoded payload findings do not fail the CLI', () => {
  write('Cargo.lock', 'version = 4\n# transitive update\n');
  write('innocent.txt', 'a'.repeat(300) + '\n');
  const head = commit();
  const run = spawnSync(process.execPath, [fileURLToPath(new URL('./check-pr.mjs', import.meta.url)), base, head], { encoding: 'utf8' });
  assert.equal(run.status, 0, run.stdout + run.stderr);
  assert.match(run.stdout, /lockfile changed without its owning manifest/);
  assert.match(run.stdout, /large encoded token/);
});

test('replacing a binary fixture with text scans new payloads and sensitive APIs', () => {
  const path = 'crates/core/src/tests/assets/replaced.txt';
  write(path, Buffer.from([0, 1, 2]));
  base = commit();
  write(path, 'reqwest::get(url);\n' + 'a'.repeat(300) + '\n');
  const result = check();
  assert.ok(result.sensitive.get(path).has('sensitive API primitive'));
  assert.ok(result.findings.some(finding =>
    finding.path === path && finding.line === 2 && finding.reason.includes('large encoded token')));
});
