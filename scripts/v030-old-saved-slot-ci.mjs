#!/usr/bin/env node

import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  CdpClient,
  V02_RELEASE_SHA256,
  assertEvidenceHasNoAbsoluteWindowsPath,
  closeMainWindowExact,
  downloadBoundAsset,
  invokeExpression,
  isolatedEnvironment,
  processRowsForExecutable,
  requireInside,
  requireNewRoot,
  resolvePublicRelease,
  runCommand,
  sha256File,
  sleep,
  waitForDebugPortClosed,
  waitForNoExecutableProcess,
  waitForTauriTarget,
  writeIntegrityEvidence,
} from "./v030-e2e-lib.mjs";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const packageName = "codex-switch-downgrade-0-2-7-downgrade-runtime-v0-2-7";
const pinnedCodexRuntime = path.join(
  repoRoot,
  "node_modules", "@openai", "codex-win32-x64", "vendor",
  "x86_64-pc-windows-msvc", "bin", "codex.exe",
);

async function freePort() {
  return await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen({ host: "127.0.0.1", port: 0, exclusive: true }, () => {
      const address = server.address();
      server.close((error) => error ? reject(error) : resolve(address.port));
    });
  });
}

async function assertNoCodexDesktopProcess() {
  const command = [
    "$rows=@(Get-Process -Name 'OpenAI.Codex' -ErrorAction SilentlyContinue)",
    "if($rows.Count -ne 0){throw 'OpenAI.Codex must be absent on the isolated CI runner'}",
  ].join(";");
  await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command]);
}

async function generateFixture(runRoot) {
  if (!fs.statSync(pinnedCodexRuntime, { throwIfNoEntry: false })?.isFile()) {
    throw new Error("the pinned Windows Codex runtime dependency is unavailable");
  }
  const runtimeVersion = await runCommand(pinnedCodexRuntime, ["--version"], {
    cwd: repoRoot,
    timeoutMs: 30_000,
  });
  if (!/^codex-cli 0\.147\.0\s*$/.test(runtimeVersion.stdout)) {
    throw new Error("the pinned Windows Codex runtime version is invalid");
  }
  await runCommand("cargo", [
    "test", "--locked", "--manifest-path", "src-tauri/Cargo.toml",
    "--test", "downgrade_old_runtime_contract",
    "exports_every_supported_v02_runtime_fixture", "--", "--ignored", "--exact", "--nocapture",
  ], {
    cwd: repoRoot,
    env: {
      ...process.env,
      CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT: runRoot,
      CODEX_SWITCH_DOWNGRADE_FIXTURE_PATCH: "7",
      CODEX_SWITCH_CODEX_RUNTIME_EXE: pinnedCodexRuntime,
    },
    timeoutMs: 20 * 60_000,
  });
}

async function verifyDatabaseContainment(database, codexHome) {
  const script = [
    "import pathlib, sqlite3, sys",
    "db=pathlib.Path(sys.argv[1]).resolve()",
    "root=pathlib.Path(sys.argv[2]).resolve()",
    "rows=sqlite3.connect(db).execute('select rollout_path from threads').fetchall()",
    "assert rows",
    "assert all(pathlib.Path(value).resolve().is_relative_to(root) for (value,) in rows)",
    "print(len(rows))",
  ].join("\n");
  const result = await runCommand("python", ["-c", script, database, codexHome], { cwd: repoRoot });
  const count = Number(result.stdout.trim());
  if (!Number.isInteger(count) || count < 1) throw new Error("old Switch database containment was not verified");
  return count;
}

async function main() {
  if (process.platform !== "win32" || process.env.GITHUB_ACTIONS !== "true" || process.env.CI !== "true") {
    throw new Error("the old saved-slot apply gate may run only on an isolated GitHub Actions Windows runner");
  }
  const argument = process.argv[2];
  if (!argument || process.argv.length !== 3) throw new Error("Usage: node scripts/v030-old-saved-slot-ci.mjs <new-absolute-run-root>");
  const runRoot = requireNewRoot(path.resolve(argument), "CI run root");
  await assertNoCodexDesktopProcess();
  await generateFixture(runRoot);

  const packageDir = requireInside(runRoot, path.join(runRoot, "packages", packageName), "v0.2.7 package");
  const executable = requireInside(packageDir, path.join(packageDir, "codex-switch.exe"), "v0.2.7 executable");
  const codexHome = requireInside(packageDir, path.join(packageDir, "codex-home"), "v0.2.7 CODEX_HOME");
  const savedConfigPath = requireInside(packageDir, path.join(packageDir, "appdata", "codex-switch", "runtimes", "plus", "config.toml"), "saved Plus config");
  const savedConfig = fs.readFileSync(savedConfigPath, "utf8");
  if (/\bsqlite_home\s*=/.test(savedConfig)) throw new Error("exported saved Plus slot retained sqlite_home");

  const release = await resolvePublicRelease("mingisrookie/codex-switch", "v0.2.7", {
    expectedSha256: V02_RELEASE_SHA256["v0.2.7"],
  });
  const download = await downloadBoundAsset(release, executable);
  const workspace = requireInside(packageDir, path.join(packageDir, "workspace"), "workspace");
  fs.mkdirSync(workspace);
  const liveConfigPath = path.join(codexHome, "config.toml");
  const packageSqlite = codexHome.replaceAll("\\", "\\\\");
  fs.writeFileSync(liveConfigPath, [
    'model = "codex-switch-old-apply-probe"',
    'model_provider = "openai_custom"',
    `sqlite_home = "${packageSqlite}"`,
    "",
    "[model_providers.openai_custom]",
    'name = "isolated old apply probe"',
    'base_url = "http://127.0.0.1:9/v1"',
    'wire_api = "responses"',
    "requires_openai_auth = false",
    "",
  ].join("\n"));
  const liveBeforeSha256 = sha256File(liveConfigPath);
  const port = await freePort();
  const environment = isolatedEnvironment(packageDir, {
    CODEX_HOME: codexHome,
    CODEX_SQLITE_HOME: codexHome,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port} --remote-allow-origins=*`,
  }, false);
  const child = spawn(executable, [], { cwd: workspace, env: environment, windowsHide: true, stdio: "ignore" });
  let cdp;
  let closed = false;
  try {
    const target = await waitForTauriTarget(port, undefined, 180_000);
    cdp = new CdpClient(target.webSocketDebuggerUrl);
    await cdp.connect();
    await cdp.call("Runtime.enable");
    let buttonState;
    const readyDeadline = Date.now() + 90_000;
    while (Date.now() < readyDeadline) {
      buttonState = await cdp.evaluate(`(()=>{const button=Array.from(document.querySelectorAll('button')).find(node=>node.textContent?.trim()==='切换到 ChatGPT 账号');return button?{found:true,disabled:button.disabled}:{found:false}})()`);
      if (buttonState.found && !buttonState.disabled) break;
      await sleep(250);
    }
    if (!buttonState?.found || buttonState.disabled) throw new Error("v0.2.7 saved Plus apply action did not become available");
    const clicked = await cdp.evaluate(`(()=>{const button=Array.from(document.querySelectorAll('button')).find(node=>node.textContent?.trim()==='切换到 ChatGPT 账号'&&!node.disabled);if(!button)return false;button.click();return true})()`);
    if (!clicked) throw new Error("v0.2.7 saved Plus apply click was not accepted");

    let liveAfter = "";
    const applyDeadline = Date.now() + 180_000;
    while (Date.now() < applyDeadline) {
      liveAfter = fs.readFileSync(liveConfigPath, "utf8");
      if (sha256File(liveConfigPath) !== liveBeforeSha256 && !/\bsqlite_home\s*=/.test(liveAfter)) break;
      await sleep(250);
    }
    if (sha256File(liveConfigPath) === liveBeforeSha256 || /\bsqlite_home\s*=/.test(liveAfter)) {
      throw new Error("v0.2.7 did not apply the sanitized saved Plus slot");
    }
    const originalCanonicalRoot = path.join(runRoot, "input", "canonical");
    if (liveAfter.toLowerCase().includes(originalCanonicalRoot.toLowerCase()) || savedConfig.toLowerCase().includes(originalCanonicalRoot.toLowerCase())) {
      throw new Error("v0.2.7 saved-slot apply reintroduced the source canonical root");
    }
    const databaseThreadCount = await verifyDatabaseContainment(path.join(codexHome, "state_5.sqlite"), codexHome);
    const runtimeStatus = await cdp.evaluate(invokeExpression("scan_runtime_status"));
    if (runtimeStatus.activeRuntimeId !== "plus") throw new Error("v0.2.7 runtime readback did not select Plus");
    await cdp.evaluate(invokeExpression("request_app_exit"), 10_000);
    cdp.close();
    cdp = undefined;
    await waitForNoExecutableProcess(executable, 90_000);
    await waitForDebugPortClosed(port, 30_000);
    closed = true;

    const payload = {
      gate: "v0.3.0-external-v0.2.7-saved-slot-apply",
      verdict: "PASS",
      observedAt: new Date().toISOString(),
      runner: "github-actions-windows",
      oldSwitch: { version: "v0.2.7", bytes: download.bytes, sha256: download.sha256, tagCommit: release.tagCommit },
      savedPlusSlot: { sanitizedBeforeApply: true, configSha256: sha256File(savedConfigPath) },
      apply: { uiAction: "switch-to-account", liveConfigChanged: true, sqliteHomeAbsentAfterApply: true, runtimeReadback: "plus" },
      database: { threadCount: databaseThreadCount, rolloutPathsContainedInPackage: true },
      safety: { codexDesktopAbsentBeforeApply: true, exactOldSwitchExited: true, debugPortClosed: true },
    };
    assertEvidenceHasNoAbsoluteWindowsPath(payload);
    const evidence = writeIntegrityEvidence(runRoot, "evidence/old-saved-slot-apply.json", payload);
    process.stdout.write(`EVIDENCE=${evidence.output}\nSHA256=${evidence.sha256}\n`);
  } finally {
    cdp?.close();
    if (!closed && (await processRowsForExecutable(executable)).length > 0) {
      await closeMainWindowExact(child.pid, executable).catch(() => {});
      await waitForNoExecutableProcess(executable, 30_000).catch(() => {});
    }
  }
}

main().catch((error) => {
  process.stderr.write(`${error.stack ?? error}\n`);
  process.exitCode = 1;
});
