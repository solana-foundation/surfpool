#!/usr/bin/env node

/// Rewrites the version of every workspace crate and of the `@solana/surfpool`
/// npm package to the given semver string, in place.
///
/// Usage: node .github/scripts/set-version.js <version>
///
/// Updates `[workspace.package].version` and the matching `surfpool-*` entries
/// in `[workspace.dependencies]` of Cargo.toml, the local `surfpool-*` entries
/// of Cargo.lock, and crates/sdk-node/package.json and package-lock.json. Only
/// entries currently at the workspace version are changed, so independently
/// versioned crates such as surfpool-studio-ui keep their own version. The
/// result still satisfies `cargo build --locked`.
///
/// Exits non-zero if the version is not valid semver or a file does not hold
/// the expected version fields.

const fs = require("fs");
const path = require("path");

const SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

const root = path.resolve(__dirname, "..", "..");
const read = (rel) => fs.readFileSync(path.join(root, rel), "utf8");
const write = (rel, text) => fs.writeFileSync(path.join(root, rel), text);
const escape = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");

function fail(message) {
  console.error(`set-version: ${message}`);
  process.exit(1);
}

function replaceExactlyOnce(text, pattern, replacement, what) {
  const matches = text.match(new RegExp(pattern.source, `${pattern.flags}g`));
  if (!matches || matches.length !== 1) {
    fail(`expected exactly one ${what}, found ${matches ? matches.length : 0}`);
  }
  return text.replace(pattern, replacement);
}

function updateCargoToml(version) {
  let toml = read("Cargo.toml");
  const current = toml.match(/^\[workspace\.package\]\s*\nversion = "([^"]+)"/m);
  if (!current) fail("Cargo.toml has no [workspace.package] version");
  const old = current[1];

  toml = replaceExactlyOnce(
    toml,
    /^(\[workspace\.package\]\s*\nversion = )"[^"]+"/m,
    `$1"${version}"`,
    "[workspace.package] version",
  );
  toml = toml.replace(
    new RegExp(`^(surfpool-[\\w-]+ = \\{.*version = )"${escape(old)}"`, "gm"),
    `$1"${version}"`,
  );
  write("Cargo.toml", toml);
  return old;
}

function updateCargoLock(old, version) {
  const lock = read("Cargo.lock");
  let changed = 0;
  // Local workspace packages are the only `[[package]]` entries without a `source`.
  const out = lock.replace(/\[\[package\]\]\n(?:[^\n]+\n)+/g, (block) => {
    const isLocalSurfpool =
      /^name = "surfpool-[\w-]+"$/m.test(block) &&
      block.includes(`\nversion = "${old}"\n`) &&
      !/^source = /m.test(block);
    if (!isLocalSurfpool) return block;
    changed += 1;
    return block.replace(`\nversion = "${old}"\n`, `\nversion = "${version}"\n`);
  });
  if (changed === 0) fail(`Cargo.lock has no local surfpool-* package at ${old}`);
  write("Cargo.lock", out);
}

function updateJson(rel, update) {
  const json = JSON.parse(read(rel));
  update(json);
  write(rel, `${JSON.stringify(json, null, 2)}\n`);
}

function main() {
  const version = process.argv[2];
  if (!version || !SEMVER.test(version)) {
    fail(`usage: set-version.js <semver>, got ${JSON.stringify(version)}`);
  }

  const old = updateCargoToml(version);
  updateCargoLock(old, version);
  updateJson("crates/sdk-node/package.json", (pkg) => {
    pkg.version = version;
  });
  updateJson("crates/sdk-node/package-lock.json", (lock) => {
    lock.version = version;
    lock.packages[""].version = version;
  });
  console.log(`set-version: ${old} -> ${version}`);
}

main();
