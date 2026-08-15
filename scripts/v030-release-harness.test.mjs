import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import test from "node:test";

import {
  assertEvidenceHasNoAbsoluteWindowsPath,
  copyBoundExecutable,
  isPathInside,
  processRowsForExecutable,
  sha256Bytes,
  stableJson,
  V02_RELEASE_SHA256,
  versionTriplet,
  writeIntegrityEvidence,
} from "./v030-e2e-lib.mjs";
import {
  OLD_RUNTIME_HELP,
  parseOldRuntimeOptions,
  runOldRuntimeE2e,
} from "./v030-old-runtime-e2e.mjs";
import {
  UPDATER_HELP,
  parseUpdaterOptions,
  runUpdaterE2e,
  startTargetLock,
} from "./v030-updater-e2e.mjs";
import { collectReleaseInputs } from "./v030-source-input-manifest.mjs";

function withTemporaryParent(callback) {
  const parent = fs.mkdtempSync(path.join(os.tmpdir(), "v030-harness-test-"));
  const cleanup = () => fs.rmSync(parent, { recursive: true, force: true });
  try {
    const result = callback(parent);
    if (result && typeof result.then === "function") return result.finally(cleanup);
    cleanup();
    return result;
  } catch (error) {
    cleanup();
    throw error;
  }
}

test("help is available without requiring paths", () => {
  assert.equal(parseOldRuntimeOptions(["--help"]).help, true);
  assert.equal(parseUpdaterOptions(["--help"]).help, true);
  assert.match(OLD_RUNTIME_HELP, /never moved/);
  assert.match(UPDATER_HELP, /production one-click updater/);
});

test("argument parsing fails closed and normalizes new absolute roots", () => {
  withTemporaryParent((parent) => {
    const oldRoot = path.join(parent, "old-runtime");
    const updateRoot = path.join(parent, "updater");
    const oldOptions = parseOldRuntimeOptions(["--run-root", oldRoot, "--dry-run", "--base-port", "24000"]);
    const updaterOptions = parseUpdaterOptions(["--run-root", updateRoot, "--dry-run", "--mode", "both"]);
    assert.equal(oldOptions.runRoot, oldRoot);
    assert.equal(updaterOptions.runRoot, updateRoot);
    assert.throws(() => parseOldRuntimeOptions(["--run-root", oldRoot, "--unknown"]), /unknown option/);
    assert.throws(() => parseUpdaterOptions(["--run-root", updateRoot, "--mode", "unsafe"]), /mode is invalid/);
    fs.mkdirSync(oldRoot);
    assert.throws(() => parseOldRuntimeOptions(["--run-root", oldRoot]), /must not already exist/);
  });
  const ignoredRoot = path.resolve("target", `v030-harness-ignored-${process.pid}`);
  const ignoredParent = path.dirname(ignoredRoot);
  const ignoredParentExisted = fs.existsSync(ignoredParent);
  fs.mkdirSync(ignoredParent, { recursive: true });
  try {
    assert.equal(parseUpdaterOptions(["--run-root", ignoredRoot, "--dry-run"]).runRoot, ignoredRoot);
  } finally {
    if (!ignoredParentExisted) fs.rmdirSync(ignoredParent);
  }
  const unignoredRoot = path.resolve(`v030-harness-unignored-${process.pid}`);
  assert.throws(() => parseUpdaterOptions(["--run-root", unignoredRoot, "--dry-run"]), /not ignored/);
});

test("containment rejects siblings, ancestors, and traversal", () => {
  const root = path.resolve("C:/isolated/v030");
  assert.equal(isPathInside(root, path.join(root, "child", "file")), true);
  assert.equal(isPathInside(root, root), false);
  assert.equal(isPathInside(root, root, true), true);
  assert.equal(isPathInside(root, path.resolve(root, "..", "sibling")), false);
});

test("integrity evidence binds canonical payload and contains no absolute path", () => {
  withTemporaryParent((parent) => {
    const runRoot = path.join(parent, "run");
    fs.mkdirSync(runRoot);
    const payload = { verdict: "PASS", nested: { count: 2 }, rows: ["a", "b"] };
    const result = writeIntegrityEvidence(runRoot, "evidence/result.json", payload);
    const document = JSON.parse(fs.readFileSync(result.output, "utf8"));
    assert.equal(document.payloadSha256, sha256Bytes(stableJson(payload)));
    assert.equal(result.payloadSha256, document.payloadSha256);
    assert.doesNotThrow(() => assertEvidenceHasNoAbsoluteWindowsPath(payload));
    assert.throws(() => assertEvidenceHasNoAbsoluteWindowsPath({ path: "C:\\Users\\fixture" }), /absolute Windows path/);
  });
});

test("PE version normalization is strict to a three-part version", () => {
  assert.equal(versionTriplet("0.2.7"), "0.2.7");
  assert.equal(versionTriplet("0.3.0.0 (release)"), "0.3.0");
  assert.equal(versionTriplet("not-a-version"), null);
});

test("every supported old runtime has an immutable SHA-256 pin", () => {
  assert.deepEqual(Object.keys(V02_RELEASE_SHA256), Array.from({ length: 8 }, (_, patch) => `v0.2.${patch}`));
  for (const digest of Object.values(V02_RELEASE_SHA256)) assert.match(digest, /^[0-9a-f]{64}$/);
});

test("local tag build copy is exclusive and SHA-256 bound", () => {
  withTemporaryParent((parent) => {
    const source = path.join(parent, "v026.exe");
    const destination = path.join(parent, "package", "codex-switch.exe");
    fs.writeFileSync(source, "pinned-v0.2.6");
    const expected = sha256Bytes(Buffer.from("pinned-v0.2.6"));

    assert.deepEqual(copyBoundExecutable(source, destination, expected), {
      bytes: Buffer.byteLength("pinned-v0.2.6"),
      sha256: expected,
    });
    assert.equal(fs.readFileSync(destination, "utf8"), "pinned-v0.2.6");
    assert.throws(() => copyBoundExecutable(source, destination, expected), /already exists/);
    assert.throws(() => copyBoundExecutable(source, path.join(parent, "bad.exe"), "0".repeat(64)), /SHA-256 pin/);
  });
});

test("replacement-lock injection blocks replacement and releases cleanly", async () => {
  await withTemporaryParent(async (parent) => {
    const target = path.join(parent, "codex-switch.exe");
    const replacement = path.join(parent, "replacement.exe");
    fs.writeFileSync(target, "old");
    fs.writeFileSync(replacement, "new");
    const lock = await startTargetLock(target, path.join(parent, "control"));
    try {
      assert.throws(() => fs.renameSync(replacement, target));
      assert.equal(fs.readFileSync(target, "utf8"), "old");
    } finally {
      await lock.release();
    }
    fs.renameSync(replacement, target);
    assert.equal(fs.readFileSync(target, "utf8"), "new");
  });
});

test("dry validation creates no run root and starts no product executable", async () => {
  await withTemporaryParent(async (parent) => {
    const oldRoot = path.join(parent, "old-dry");
    const updaterRoot = path.join(parent, "update-dry");
    const old = await runOldRuntimeE2e(parseOldRuntimeOptions(["--run-root", oldRoot, "--dry-run"]));
    const updater = await runUpdaterE2e(parseUpdaterOptions(["--run-root", updaterRoot, "--dry-run"]));
    assert.equal(old.verdict, "DRY_RUN_PASS");
    assert.equal(updater.verdict, "DRY_RUN_PASS");
    assert.equal(fs.existsSync(oldRoot), false);
    assert.equal(fs.existsSync(updaterRoot), false);
  });
});

test("native helper exposes help and all harnesses avoid broad taskkill", () => {
  const python = spawnSync("python", ["scripts/v030-old-runtime-e2e-native.py", "--help"], {
    cwd: path.resolve("."),
    encoding: "utf8",
    windowsHide: true,
  });
  assert.equal(python.status, 0, python.stderr);
  assert.match(python.stdout, /validate package containment/i);
  for (const file of [
    "scripts/v030-e2e-lib.mjs",
    "scripts/v030-old-runtime-e2e.mjs",
    "scripts/v030-old-runtime-e2e-native.py",
    "scripts/v030-old-saved-slot-ci.mjs",
    "scripts/v030-product-ui-e2e.mjs",
    "scripts/v030-updater-e2e.mjs",
  ]) {
    const source = fs.readFileSync(file, "utf8");
    assert.doesNotMatch(source, /taskkill(?:\.exe)?/i);
    assert.doesNotMatch(source, /Stop-Process/i);
  }
});

test("exact process inventory tolerates an absent or already-exited executable", async () => {
  if (process.platform !== "win32") return;
  const missing = path.join(os.tmpdir(), `v030-no-process-${process.pid}`, "codex-switch.exe");
  assert.deepEqual(await processRowsForExecutable(missing), []);
});

test("product UI gate seeds a complete runtime and re-navigates after CDP subscription", () => {
  const source = fs.readFileSync("scripts/v030-product-ui-e2e.mjs", "utf8");
  assert.match(source, /config\.toml[\s\S]*model_provider = "openai"/);
  assert.match(source, /Runtime\.enable[\s\S]*Page\.enable[\s\S]*Page\.navigate/);
  assert.doesNotMatch(source, /Page\.reload/);
});

test("production Relay switching exposes no link-status verifier or pre-switch probe", () => {
  const app = fs.readFileSync("src/App.tsx", "utf8");
  const api = fs.readFileSync("src/api.ts", "utf8");
  const commands = fs.readFileSync("src-tauri/src/commands.rs", "utf8");
  const lib = fs.readFileSync("src-tauri/src/lib.rs", "utf8");

  assert.equal(fs.existsSync("src-tauri/src/relay_verify.rs"), false);
  assert.doesNotMatch(app, /verifyRelayRuntime|验证连接后切换|选择中转站切换方式/);
  assert.doesNotMatch(api, /test_relay_connection|verifyRelayRuntime/);
  assert.doesNotMatch(commands, /verify_relay_for_switch|test_relay_connection/);
  assert.doesNotMatch(lib, /relay_verify|test_relay_connection/);
  assert.match(commands, /relay_validation_without_network/);
});

test("old saved-slot apply gate refuses unhosted invocations that did not opt in", () => {
  const runRoot = path.join(os.tmpdir(), `v030-old-saved-slot-refused-${process.pid}`);
  const result = spawnSync(process.execPath, ["scripts/v030-old-saved-slot-ci.mjs", runRoot], {
    cwd: path.resolve("."),
    encoding: "utf8",
    env: { ...process.env, GITHUB_ACTIONS: "false", CI: "false", V030_ALLOW_LOCAL_GATE: "" },
    windowsHide: true,
  });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /isolated GitHub Actions Windows runner or V030_ALLOW_LOCAL_GATE=1/);
  assert.equal(fs.existsSync(runRoot), false);
});

test("old saved-slot CI gate binds an exact dev-only native Codex runtime", () => {
  const packageJson = JSON.parse(fs.readFileSync("package.json", "utf8"));
  assert.equal(packageJson.devDependencies?.["@openai/codex"], "0.147.0");
  const source = fs.readFileSync("scripts/v030-old-saved-slot-ci.mjs", "utf8");
  assert.match(source, /codex-win32-x64/);
  assert.match(source, /CODEX_SWITCH_CODEX_RUNTIME_EXE: pinnedCodexRuntime/);
  assert.match(source, /\^codex-cli 0\\\.147\\\.0/);
});

test("every product launch in the release harness is observed, never stdio-ignored", () => {
  for (const file of [
    "scripts/v030-old-saved-slot-ci.mjs",
    "scripts/v030-e2e-lib.mjs",
    "scripts/v030-product-ui-e2e.mjs",
  ]) {
    const source = fs.readFileSync(file, "utf8");
    const offender = source.split("\n").findIndex((line) => /windowsHide: true, stdio: "ignore"|^\s*stdio: "ignore",/.test(line));
    assert.equal(offender, -1, `${file}:${offender + 1} launches the product with discarded output`);
  }
  const lib = fs.readFileSync("scripts/v030-e2e-lib.mjs", "utf8");
  assert.match(lib, /export function spawnObservedProduct/);
  assert.match(lib, /export async function captureLaunchDiagnostics/);
  assert.match(lib, /product exited before serving CDP/);
});

test("CDP target discovery tries every local loopback address family", () => {
  const source = fs.readFileSync("scripts/v030-e2e-lib.mjs", "utf8");
  assert.match(source, /127\.0\.0\.1/);
  assert.match(source, /localhost/);
  assert.match(source, /\[::1\]/);
});

test("the old saved-slot gate injects debugging switches through both WebView2 channels", () => {
  const source = fs.readFileSync("scripts/v030-old-saved-slot-ci.mjs", "utf8");
  // The environment variable alone is not enough: hosted runners receive it and
  // decline to act on it, so the host command line channel must be present too.
  assert.match(source, /WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: debuggingSwitches/);
  assert.match(source, /--edge-webview-switches=\$\{debuggingSwitches\}/);
  assert.match(source, /WEBVIEW2_USER_DATA_FOLDER: webViewProfile/);
  const lib = fs.readFileSync("scripts/v030-e2e-lib.mjs", "utf8");
  assert.match(lib, /export async function probeChildEnvironment/);
});

test("the saved-slot gate asserts sqlite_home containment rather than absence", () => {
  const source = fs.readFileSync("scripts/v030-old-saved-slot-ci.mjs", "utf8");
  // v0.2.7 owns sqlite_home and rewrites it to the home it manages, so absence is
  // not a property the old runtime can satisfy. The gate must still refuse a value
  // that escapes the isolated package, and must still refuse the tampered slot.
  assert.match(source, /isPathInside\(packageDir, path\.resolve\(value/);
  assert.match(source, /sqliteHomeEscapedPackage/);
  assert.match(source, /exported saved Plus slot retained sqlite_home/);
  assert.match(source, /saved-slot apply reintroduced the source canonical root/);
  assert.doesNotMatch(source, /sqliteHomeAbsentAfterApply/);
});

test("source-input manifest covers release inputs and excludes generated artifacts", () => {
  const inputs = collectReleaseInputs();
  const names = new Set(inputs.map((entry) => entry.path));
  assert.equal(names.has("scripts/v030-source-input-manifest.mjs"), true);
  assert.equal(names.has("src-tauri/Cargo.toml"), true);
  assert.equal(names.has("src-tauri/capabilities/default.json"), true);
  assert.equal(names.has("src-tauri/icons/icon.ico"), true);
  assert.equal(names.has("src-tauri/resources/skills/grok-search/SKILL.md"), true);
  assert.equal(names.has("public/favicon.ico"), true);
  assert.equal(names.has("rust-toolchain.toml"), true);
  assert.equal(names.has("src/App.tsx"), true);
  assert.equal(
    names.has(".trellis/spec/backend/database-guidelines.md"),
    fs.existsSync(path.resolve(".trellis/spec")),
  );
  assert.equal(names.has("docs/USER_GUIDE.md"), true);
  assert.equal(names.has("docs/SECURITY.md"), true);
  assert.equal([...names].some((name) => name.startsWith("src-tauri/target/")), false);
  assert.equal([...names].some((name) => name.includes("/evidence/")), false);
  for (const input of inputs) {
    assert.match(input.sha256, /^[0-9a-f]{64}$/);
    assert.ok(input.bytes > 0, input.path);
  }
});
