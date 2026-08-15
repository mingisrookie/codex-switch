#!/usr/bin/env node

import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

import { sha256Bytes, stableJson } from "./v030-e2e-lib.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function git(...args) {
  return execFileSync("git", args, { cwd: repoRoot, encoding: "utf8", windowsHide: true });
}

function isReleaseInput(relativePath) {
  const normalized = relativePath.replaceAll("\\", "/");
  if (normalized.startsWith(".github/")) return true;
  if (normalized.startsWith(".trellis/spec/")) return true;
  if (normalized.startsWith("scripts/")) return true;
  if (normalized.startsWith("src/")) return true;
  if (normalized.startsWith("src-tauri/")
      && !normalized.startsWith("src-tauri/target/")
      && !normalized.startsWith("src-tauri/gen/")) return true;
  if (normalized.startsWith("public/")) return true;
  if ([
    "rust-toolchain.toml",
    ".gitattributes",
    ".gitignore",
    "AGENTS.md",
    "CHANGELOG.md",
    "README.md",
    "docs/USER_GUIDE.md",
    "docs/SECURITY.md",
    "docs/SKILLS.md",
    "docs/RELEASE_VERIFICATION.md",
    "index.html",
    "package.json",
    "package-lock.json",
    "tsconfig.json",
    "tsconfig.node.json",
    "vite.config.ts",
  ].includes(normalized)) return true;
  return !normalized.includes("/") && normalized.endsWith(".md");
}

function sha256File(file) {
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(file));
  return hash.digest("hex");
}

function regularFilesUnder(relativeRoot) {
  const files = [];
  if (!fs.existsSync(path.resolve(repoRoot, relativeRoot))) return files;
  const visit = (relativeDirectory) => {
    const absoluteDirectory = path.resolve(repoRoot, relativeDirectory);
    for (const entry of fs.readdirSync(absoluteDirectory, { withFileTypes: true })) {
      const relativePath = path.join(relativeDirectory, entry.name);
      if (entry.isSymbolicLink()) throw new Error(`release input directory contains a symlink: ${relativePath}`);
      if (entry.isDirectory()) visit(relativePath);
      else if (entry.isFile()) files.push(relativePath);
    }
  };
  visit(relativeRoot);
  return files;
}

export function collectReleaseInputs() {
  const repositoryPaths = git("ls-files", "--cached", "--others", "--exclude-standard", "-z")
    .split("\0")
    .filter(Boolean)
    .filter((relativePath) => fs.existsSync(path.resolve(repoRoot, relativePath)))
    .filter(isReleaseInput);
  const paths = [...new Set([
    ...repositoryPaths,
    ...regularFilesUnder(".trellis/spec"),
  ])]
    .sort((left, right) => left.localeCompare(right, "en"));
  if (paths.length === 0) throw new Error("release input inventory is empty");
  return paths.map((relativePath) => {
    const absolutePath = path.resolve(repoRoot, relativePath);
    const stat = fs.lstatSync(absolutePath);
    if (!stat.isFile() || stat.isSymbolicLink()) throw new Error(`release input is not a regular file: ${relativePath}`);
    return {
      path: relativePath.replaceAll("\\", "/"),
      bytes: stat.size,
      sha256: sha256File(absolutePath),
    };
  });
}

export function createSourceInputManifest() {
  const inputs = collectReleaseInputs();
  const inputSetSha256 = sha256Bytes(stableJson(inputs));
  return {
    schemaVersion: 1,
    gate: "v0.3.0-frozen-source-inputs",
    observedAt: new Date().toISOString(),
    gitHead: git("rev-parse", "HEAD").trim(),
    inputCount: inputs.length,
    inputSetSha256,
    inputs,
  };
}

const USAGE = "Usage: node scripts/v030-source-input-manifest.mjs <absolute-output.json>";

function main() {
  // Without this, --help is taken as the output path and the manifest is written
  // to a file literally named "--help" in the working directory.
  if (process.argv.includes("--help") || process.argv.includes("-h")) {
    process.stdout.write(`${USAGE}\n`);
    return;
  }
  if (process.argv.length !== 3) throw new Error(USAGE);
  const output = path.resolve(process.argv[2]);
  if (!path.isAbsolute(output)) throw new Error("source manifest output must be absolute");
  if (isReleaseInput(path.relative(repoRoot, output))) throw new Error("source manifest output must not be a release input");
  fs.mkdirSync(path.dirname(output), { recursive: true });
  const payload = createSourceInputManifest();
  const document = { ...payload, payloadSha256: sha256Bytes(stableJson(payload)) };
  fs.writeFileSync(output, `${JSON.stringify(document, null, 2)}\n`, { encoding: "utf8", flag: "wx" });
  process.stdout.write(`INPUT_SET_SHA256=${payload.inputSetSha256}\nOUTPUT_SHA256=${sha256File(output)}\n`);
}

if (path.resolve(process.argv[1] ?? "") === fileURLToPath(import.meta.url)) {
  try {
    main();
  } catch (error) {
    process.stderr.write(`${error.stack ?? error}\n`);
    process.exitCode = 1;
  }
}
