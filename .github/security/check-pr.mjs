import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { posix, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { TextDecoder } from 'node:util';

const OWNER = '@MicaiahReid';
const MAX_BLOB_BYTES = 10 * 1024 * 1024;
const POLICY_PATH = new URL('../CODEOWNERS', import.meta.url);
const WORKSPACE_MANIFESTS = JSON.parse(readFileSync(new URL('./workspace-manifests.json', import.meta.url), 'utf8'));
const NODE_LOCKS = new Set(['package-lock.json', 'npm-shrinkwrap.json', 'yarn.lock', 'pnpm-lock.yaml', 'bun.lock', 'bun.lockb']);
const HASH_LOCKS = new Set(['Cargo.lock', ...NODE_LOCKS, 'poetry.lock', 'uv.lock', 'Pipfile.lock', 'go.sum']);
const ESCAPES = /(?:\\x[0-9a-fA-F]{2}|\\u[0-9a-fA-F]{4}){8,}/;
const OBFUSCATION = /\b(?:eval|exec|atob)\s*\(|\b(?:Function|fromCharCode)\s*\(|\b(?:base64\s*(?:::|\.)\s*(?:decode|b64decode)|b64decode)\s*\(/;
const SENSITIVE_API = /\b(?:std|tokio)\s*::\s*(?:fs|process|net)\b|\b(?:reqwest|ureq|hyper|Keypair|Signer|TcpStream|TcpListener)\b|\b(?:Command|File|OpenOptions)\s*::|\b(?:fetch|require|readFile|writeFile|execSync|spawn|sign_message|encrypt|decrypt)\s*\(|(?:node:|from\s+['"])(?:fs|child_process|net|http|https|crypto)\b|\b(?:import|from)\s+(?:os|subprocess|socket|requests|urllib|shutil)\b|\b(?:curl|wget)\s+https?:\/\//;

/** Return Git output without invoking a shell or interpreting pathspec magic. */
function git(...args) {
  return execFileSync('git', ['--literal-pathspecs', ...args], { maxBuffer: 64 * 1024 * 1024 });
}

/** Read a bounded repository blob without checking it out or following symlinks. */
function blob(revision, path) {
  const spec = `${revision}:${path}`;
  if (Number(git('cat-file', '-s', spec)) > MAX_BLOB_BYTES) {
    throw new Error('blob exceeds the 10 MiB inspection limit');
  }
  return git('show', spec);
}

/** Match basename globs; wildcard characters never match a path separator. */
function globMatches(value, pattern) {
  const escaped = pattern.replace(/[.+^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`^${escaped.replaceAll('*', '[^/]*').replaceAll('?', '[^/]')}$`).test(value);
}

/** Return whether the last matching CODEOWNERS rule assigns only the configured owner. */
function owned(path, policy) {
  let owners = [];
  for (const line of policy.split(/\r?\n/)) {
    const fields = line.split('#', 1)[0].trim().split(/\s+/);
    if (!fields[0]) continue;
    const [rule, ...ruleOwners] = fields;
    const rooted = rule.startsWith('/');
    const directory = rule.endsWith('/');
    const pattern = rule.replace(/^\/+|\/+$/g, '');
    if (/[!\[\]\\]/.test(pattern) || (pattern.includes('/') && /[*?]/.test(pattern))) {
      throw new Error(`unsupported CODEOWNERS pattern: ${pattern}`);
    }
    const parts = path.split('/');
    const matches = rooted || pattern.includes('/')
      ? path === pattern || path.startsWith(`${pattern}/`)
      : (directory ? parts.slice(0, -1) : parts).some(part => globMatches(part, pattern));
    if (matches) owners = ruleOwners;
  }
  return owners.length === 1 && owners[0] === OWNER;
}

/** Yield head line numbers and added text for a changed path. */
function* addedLines(base, head, path) {
  // The base blob may be binary even when the head blob is inspectable text.
  const diff = git('diff', '--text', '--no-ext-diff', '--no-textconv', '--unified=0', '--no-renames', base, head, '--', path).toString('utf8');
  let number;
  for (const line of diff.split('\n')) {
    const hunk = line.match(/^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/);
    if (hunk) number = Number(hunk[1]);
    else if (number !== undefined) {
      if (line.startsWith('+')) yield [number++, line.slice(1)];
      else if (line.startsWith(' ')) number++;
    }
  }
}

/** Return payload indicators without disclosing string values. */
function payloadReasons(line) {
  const reasons = new Set();
  if (line.length > 2000) reasons.add('line over 2,000 characters (minified code or embedded data)');
  if (ESCAPES.test(line)) reasons.add('eight or more consecutive hex/Unicode escapes');
  if (OBFUSCATION.test(line)) reasons.add('dynamic execution or decoding primitive');
  for (const [token] of line.matchAll(/[A-Za-z0-9+/_=-]{80,}/g)) {
    const counts = new Map();
    for (const char of token) counts.set(char, (counts.get(char) ?? 0) + 1);
    const entropy = -[...counts.values()].reduce((sum, count) => {
      const probability = count / token.length;
      return sum + probability * Math.log2(probability);
    }, 0);
    if (token.length >= 256 || entropy >= 4.5) reasons.add('large encoded token or high-entropy string');
  }
  return reasons;
}

/** Report changed lockfiles without a corresponding manifest change. */
function lockfileFindings(changed) {
  const findings = [];
  for (const path of [...changed].sort()) {
    const name = posix.basename(path);
    const sibling = name => posix.join(posix.dirname(path), name);
    let manifests;
    if (name === 'Cargo.lock') {
      // The trusted list prevents excluded examples from satisfying the workspace lockfile check.
      manifests = path === 'Cargo.lock' ? WORKSPACE_MANIFESTS : [sibling('Cargo.toml')];
    } else if (NODE_LOCKS.has(name)) manifests = [sibling('package.json')];
    else if (name === 'poetry.lock' || name === 'uv.lock') manifests = [sibling('pyproject.toml')];
    else if (name === 'Pipfile.lock') manifests = [sibling('Pipfile')];
    else if (name === 'go.sum') manifests = [sibling('go.mod')];
    else continue;
    if (!manifests.some(manifest => changed.has(manifest))) {
      findings.push({ path, line: 0, reason: 'lockfile changed without its owning manifest' });
    }
  }
  return findings;
}

/**
 * Inspect committed PR changes using a trusted CODEOWNERS policy.
 * @param {string} base - Commit at the PR merge base.
 * @param {string} head - PR head commit.
 * @param {string} policy - Trusted CODEOWNERS contents.
 * @returns {{changed: Set<string>, sensitive: Map<string, Set<string>>, findings: Array<{path: string, line: number, reason: string}>}} Paths, indicators, and advisory findings.
 */
export function inspect(base, head, policy) {
  const paths = (...args) => new Set(git('diff', '--no-renames', ...args, '--name-only', '-z', base, head).toString('utf8').split('\0').filter(Boolean));
  const changed = paths();
  const deleted = paths('--diff-filter=D');
  const sensitive = new Map();
  const findings = lockfileFindings(changed);
  const flag = (path, reason) => {
    if (!sensitive.has(path)) sensitive.set(path, new Set());
    sensitive.get(path).add(reason);
  };
  const mode = (revision, path) => git('ls-tree', revision, '--', path).toString('utf8').split(' ', 1)[0];
  for (const path of [...changed].sort()) {
    const covered = owned(path, policy);
    if (covered) flag(path, `CODEOWNERS: ${OWNER}`);
    const revision = deleted.has(path) ? base : head;
    const fileMode = mode(revision, path);
    if (fileMode === '120000' || fileMode === '160000') {
      const reason = 'symlink or submodule requires owner review';
      flag(path, reason);
      if (!covered) findings.push({ path, line: 0, reason: `${reason}; no matching base CODEOWNERS entry` });
      continue;
    }
    if (fileMode === '100755' && (deleted.has(path) || mode(base, path) !== fileMode)) {
      flag(path, 'executable file added or removed');
      if (!covered) findings.push({ path, line: 0, reason: 'executable change outside base CODEOWNERS' });
    }
    const spec = `${revision}:${path}`;
    if (Number(git('cat-file', '-s', spec)) > MAX_BLOB_BYTES) {
      flag(path, 'blob exceeds the 10 MiB inspection limit; content not scanned');
      findings.push({ path, line: 0, reason: 'blob exceeds the 10 MiB inspection limit; content not scanned' });
      continue;
    }
    const data = blob(revision, path);
    let binary;
    try {
      binary = new TextDecoder('utf-8', { fatal: true }).decode(data).includes('\0');
    } catch {
      binary = true;
    }
    if (binary) {
      const reason = 'binary requires owner review';
      flag(path, reason);
      if (!covered) findings.push({ path, line: 0, reason: `${reason}; no matching base CODEOWNERS entry` });
      continue;
    }
    if (deleted.has(path)) continue;
    for (const [number, line] of addedLines(base, head, path)) {
      // Integrity hashes are expected encoded data; dependency review and ownership still apply.
      if (!HASH_LOCKS.has(posix.basename(path))) {
        for (const reason of payloadReasons(line)) {
          flag(path, reason);
          findings.push({ path, line: number, reason });
        }
      }
      if (SENSITIVE_API.test(line)) {
        flag(path, 'sensitive API primitive');
        if (!covered) findings.push({ path, line: number, reason: 'sensitive API outside base CODEOWNERS; no matching base CODEOWNERS entry' });
      }
    }
  }
  return { changed, sensitive, findings };
}

/** Escape untrusted paths for Markdown tables and workflow summaries. */
function safePath(path) {
  return path.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')
    .replaceAll('|', '&#124;').replaceAll('\n', '&#10;').replaceAll('\r', '&#13;');
}

/**
 * Format a first-glance security signal without prescribing review depth.
 * @param {ReturnType<typeof inspect>} result - Inspected paths, indicators, and findings.
 * @returns {string} Markdown for the Actions run summary.
 */
export function formatSummary({ changed, sensitive, findings }) {
  const summary = ['## PR security signals', '', sensitive.size
    ? '**Potentially sensitive changes detected.**'
    : '**No configured sensitive-change indicators detected.**', '',
  'These indicators help focus an initial inspection; they do not determine the scope of human review.', '',
  '| Path | Indicators |', '| --- | --- |'];
  for (const path of [...changed].sort()) {
    const reason = sensitive.has(path) ? [...sensitive.get(path)].sort().join('; ') : 'No configured indicators detected';
    summary.push(`| ${safePath(path)} | ${reason} |`);
  }
  summary.push('', `Advisory findings: ${findings.length}.`, '');
  for (const { path, line, reason } of findings) summary.push(`- ${safePath(path)}:${line}: ${reason}`);
  return summary.join('\n') + '\n';
}

/** Check full base/head SHAs and publish indicators and advisory findings. */
function main() {
  const revisions = process.argv.slice(2);
  if (revisions.length !== 2 || !revisions.every(sha => /^[0-9a-f]{40}$/.test(sha))) {
    throw new Error('expected full base and head commit SHAs');
  }
  const [base, head] = revisions;
  const mergeBase = git('merge-base', base, head).toString('utf8').trim();
  const result = inspect(mergeBase, head, readFileSync(POLICY_PATH, 'utf8'));
  const summary = formatSummary(result);
  for (const finding of result.findings) console.log(JSON.stringify(finding));
  if (process.env.GITHUB_STEP_SUMMARY) writeFileSync(process.env.GITHUB_STEP_SUMMARY, summary);
  else console.log(summary);
  process.exitCode = 0;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
