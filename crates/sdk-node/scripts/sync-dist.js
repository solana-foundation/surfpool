const fs = require("node:fs");
const path = require("node:path");

const rootDir = path.resolve(__dirname, "..");
const sourceDir = path.join(rootDir, "surfpool-sdk");
const distDir = path.join(rootDir, "dist");

fs.mkdirSync(distDir, { recursive: true });

for (const entry of fs.readdirSync(sourceDir)) {
  if (
    entry === "internal.js" ||
    entry === "internal.d.ts" ||
    entry.endsWith(".node")
  ) {
    fs.copyFileSync(path.join(sourceDir, entry), path.join(distDir, entry));
  }
}

// The package is CJS ("type" unset); dist/kit/esm holds ESM output, so it
// needs its own package.json to flip the module type for everything below it.
const kitEsmDir = path.join(distDir, "kit", "esm");
if (fs.existsSync(kitEsmDir)) {
  fs.writeFileSync(
    path.join(kitEsmDir, "package.json"),
    JSON.stringify({ type: "module" }, null, 2) + "\n",
  );
}
