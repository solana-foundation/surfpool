#!/usr/bin/env node

const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const PLATFORM_PACKAGES = [
  "surfpool-sdk-darwin-x64",
  "surfpool-sdk-darwin-arm64",
  "surfpool-sdk-linux-x64-gnu",
];

const VERSION_PATTERN =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

function usage() {
  console.error("Usage: npm run prepare-release -- <version>");
  console.error("Example: npm run prepare-release -- 1.2.0");
}

function readJson(filePath) {
  return JSON.parse(fs.readFileSync(filePath, "utf8"));
}

function writeJson(filePath, value) {
  fs.writeFileSync(filePath, `${JSON.stringify(value, null, 2)}\n`);
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    stdio: "inherit",
    ...options,
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed`);
  }
}

function getNpmVersion() {
  const result = spawnSync("npm", ["--version"], {
    encoding: "utf8",
  });

  if (result.error) {
    throw result.error;
  }

  if (result.status !== 0) {
    throw new Error("npm --version failed");
  }

  return result.stdout.trim();
}

function assertNpmVersionMatchesCi() {
  const npmVersion = getNpmVersion();
  const [major, minor] = npmVersion.split(".").map(Number);
  if (major !== 11 || minor < 10) {
    throw new Error(
      `Expected npm 11.10.x or newer to match CI, found ${npmVersion}. ` +
        "Run `nvm use 22.14.0 && npm install -g npm@^11.10.0` first.",
    );
  }
}

function updateOptionalDependencies(optionalDependencies, version) {
  if (!optionalDependencies) {
    throw new Error("package.json is missing optionalDependencies");
  }

  for (const packageName of PLATFORM_PACKAGES) {
    if (!Object.hasOwn(optionalDependencies, packageName)) {
      throw new Error(`optionalDependencies is missing ${packageName}`);
    }
    optionalDependencies[packageName] = version;
  }
}

function main() {
  const version = process.argv[2];
  if (!version || process.argv.length > 3) {
    usage();
    process.exit(1);
  }

  if (!VERSION_PATTERN.test(version)) {
    console.error(`Invalid npm version: ${version}`);
    usage();
    process.exit(1);
  }

  const packageDir = path.resolve(__dirname, "..");
  const packageJsonPath = path.join(packageDir, "package.json");

  assertNpmVersionMatchesCi();

  const packageJson = readJson(packageJsonPath);
  packageJson.version = version;
  updateOptionalDependencies(packageJson.optionalDependencies, version);
  writeJson(packageJsonPath, packageJson);

  run("npm", ["install", "--package-lock-only", "--ignore-scripts"], {
    cwd: packageDir,
  });

  console.log(`Prepared @solana/surfpool npm release ${version}`);
}

main();
