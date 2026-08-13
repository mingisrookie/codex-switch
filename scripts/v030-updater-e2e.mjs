#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import {
  CdpClient,
  V02_RELEASE_SHA256,
  assertEvidenceHasNoAbsoluteWindowsPath,
  assertOnlyFlags,
  downloadBoundAsset,
  fetchGithubJson,
  invokeExpression,
  isMain,
  isolatedEnvironment,
  parseArgs,
  processRowsForExecutable,
  readPeVersion,
  relativeEvidencePath,
  requireInside,
  requireIgnoredEvidenceRoot,
  resolvePublicRelease,
  runCommand,
  sha256File,
  sleep,
  versionTriplet,
  waitForNoExecutableProcess,
  waitForDebugPortClosed,
  waitForTauriTarget,
  writeIntegrityEvidence,
} from "./v030-e2e-lib.mjs";

const DEFAULT_REPOSITORY = "mingisrookie/codex-switch";
const OLD_TAG = "v0.2.7";
const NEW_TAG = "v0.3.0";
const MODES = new Set(["success", "replacement-lock-rollback", "both"]);

export const UPDATER_HELP = `
Usage:
  node scripts/v030-updater-e2e.mjs --run-root <absolute-new-directory> [options]

Options:
  --run-root <path>       New isolated updater evidence root.
  --repository <owner/repo>  Public release repository (default: ${DEFAULT_REPOSITORY}).
  --mode <mode>           success | replacement-lock-rollback | both (default: both).
  --base-port <port>      First WebView2 debugging port (default: random high port).
  --dry-run               Validate local prerequisites only; no network, files, or EXEs.
  --help                  Show this help.

The live gate downloads public v0.2.7 and v0.3.0 assets, starts v0.2.7 from its
final isolated install path, invokes the production one-click updater, proves
same-path PID/Ready/ACK transition and zero residue, then repeats with an
external exact-target file handle to force and verify production rollback.
No updater trust bypass or production failure-injection hook is used.
`;

export function parseUpdaterOptions(argv) {
  const { values, flags } = parseArgs(argv, new Set(["run-root", "repository", "mode", "base-port"]));
  assertOnlyFlags(flags, new Set(["help", "dry-run"]));
  if (flags.has("help")) return { help: true };
  const runRoot = requireIgnoredEvidenceRoot(values.get("run-root"), "run root");
  const repository = values.get("repository") ?? DEFAULT_REPOSITORY;
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository)) throw new Error("repository is invalid");
  const mode = values.get("mode") ?? "both";
  if (!MODES.has(mode)) throw new Error("updater mode is invalid");
  const basePort = values.has("base-port") ? Number(values.get("base-port")) : 34_000 + Math.floor(Math.random() * 8_000);
  if (!Number.isInteger(basePort) || basePort < 10_000 || basePort > 65_000 - 1) throw new Error("base port is invalid");
  return { help: false, dryRun: flags.has("dry-run"), runRoot, repository, mode, basePort };
}

function protectedState(root, scenario) {
  const files = [
    ["codex-home", "protected-state.bin"],
    ["appdata", "codex-switch", "protected-state.bin"],
    ["install", "protected-state.bin"],
  ].map((parts) => requireInside(root, path.join(root, ...parts), "protected state"));
  for (const [index, file] of files.entries()) {
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(file, `synthetic-v030-updater-protected-${scenario}-${index}\n`, { flag: "wx" });
  }
  return () => files.map((file, index) => ({
    label: ["codexHome", "appdata", "installSibling"][index],
    bytes: fs.statSync(file).size,
    sha256: sha256File(file),
  }));
}

export async function startTargetLock(target, controlRoot) {
  const stopPath = requireInside(controlRoot, path.join(controlRoot, "release-target-lock"), "lock stop signal");
  fs.mkdirSync(controlRoot, { recursive: true });
  const command = [
    "$ErrorActionPreference='Stop'",
    "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)",
    "$stream=[IO.File]::Open($env:V030_LOCK_TARGET,[IO.FileMode]::Open,[IO.FileAccess]::Read,[IO.FileShare]::Read)",
    "[Console]::Out.WriteLine('READY')",
    "[Console]::Out.Flush()",
    "try{while(-not (Test-Path -LiteralPath $env:V030_LOCK_STOP)){Start-Sleep -Milliseconds 100}}finally{$stream.Dispose()}",
  ].join(";");
  const child = spawn("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command], {
    env: { ...process.env, V030_LOCK_TARGET: target, V030_LOCK_STOP: stopPath },
    windowsHide: true,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stderr = "";
  child.stderr.on("data", (chunk) => { stderr += chunk.toString("utf8"); });
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("target lock holder did not become ready")), 15_000);
    child.stdout.on("data", (chunk) => {
      if (chunk.toString("utf8").includes("READY")) {
        clearTimeout(timer);
        resolve();
      }
    });
    child.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`target lock holder exited early (${code}): ${stderr.slice(-300)}`));
    });
    child.once("error", reject);
  });
  return {
    pid: child.pid,
    async release() {
      fs.writeFileSync(stopPath, "release\n", { flag: "wx" });
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error("target lock holder did not exit after release")), 15_000);
        child.once("exit", (code) => {
          clearTimeout(timer);
          if (code === 0) resolve();
          else reject(new Error(`target lock holder failed (${code})`));
        });
      });
    },
  };
}

async function waitForOriginalPidExit(executable, originalPid, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const rows = await processRowsForExecutable(executable);
    if (!rows.some((row) => row.pid === originalPid)) return;
    await sleep(200);
  }
  throw new Error("the original updater parent PID did not exit");
}

async function waitForRestartedApp(port, expectedVersion, expectedNotice, timeoutMs = 90_000) {
  const deadline = Date.now() + timeoutMs;
  let lastError = "";
  while (Date.now() < deadline) {
    try {
      const target = await waitForTauriTarget(port, undefined, 3_000);
      const cdp = new CdpClient(target.webSocketDebuggerUrl);
      await cdp.connect();
      try {
        const state = await cdp.evaluate(`(async()=>{
          const invoke=window.__TAURI_INTERNALS__?.invoke??window.__TAURI__?.core?.invoke??window.__TAURI__?.invoke;
          const app=await invoke("get_app_status");
          const notice=await invoke("get_update_startup_notice");
          return {version:app.version,notice,readyState:document.readyState,title:document.title,bodyTextLength:document.body?.innerText?.length??0};
        })()`);
        if (
          state.version === expectedVersion
          && state.notice?.status === expectedNotice
          && state.readyState === "complete"
          && state.bodyTextLength >= 50
        ) return { cdp, target, state };
        lastError = `unexpected restarted version ${state.version}`;
      } catch (error) {
        lastError = error.message;
      }
      cdp.close();
    } catch (error) {
      lastError = error.message;
    }
    await sleep(250);
  }
  throw new Error(`restarted application did not become Ready: ${lastError}`);
}

function updaterResidues(scenarioRoot) {
  const temp = path.join(scenarioRoot, "temp");
  const install = path.join(scenarioRoot, "install");
  const staging = fs.existsSync(temp)
    ? fs.readdirSync(temp).filter((name) => name.startsWith("codex-switch-update-"))
    : [];
  const installArtifacts = fs.existsSync(install)
    ? fs.readdirSync(install).filter((name) => /^\.codex-switch\.exe\.update-(?:new|backup)-/.test(name))
    : [];
  return { staging, installArtifacts };
}

async function waitForNoUpdaterResidue(scenarioRoot, timeoutMs = 45_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const residues = updaterResidues(scenarioRoot);
    if (residues.staging.length === 0 && residues.installArtifacts.length === 0) return;
    await sleep(250);
  }
  throw new Error("updater helper/staging residue remained after startup ACK");
}

async function countProcessesInside(root) {
  const command = [
    "$ErrorActionPreference='Stop'",
    "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)",
    "$root=[IO.Path]::GetFullPath($env:V030_PROCESS_ROOT).TrimEnd('\\')+'\\'",
    "$rows=@(Get-Process -ErrorAction SilentlyContinue|ForEach-Object{try{$candidate=$_.Path}catch{return};if($candidate){$full=[IO.Path]::GetFullPath($candidate);if($full.StartsWith($root,[StringComparison]::OrdinalIgnoreCase)){[ordered]@{pid=$_.Id;name=$_.ProcessName}}}})",
    "@($rows)|ConvertTo-Json -Compress",
  ].join(";");
  const result = await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command], {
    env: { ...process.env, V030_PROCESS_ROOT: root },
  });
  const parsed = JSON.parse(result.stdout.trim() || "[]");
  return Array.isArray(parsed) ? parsed : [parsed];
}

async function runScenario(runRoot, name, port, oldRelease, newRelease) {
  const rollback = name === "replacement-lock-rollback";
  const scenarioRoot = requireInside(runRoot, path.join(runRoot, "scenarios", name), "scenario root");
  const target = requireInside(scenarioRoot, path.join(scenarioRoot, "install", "codex-switch.exe"), "installed executable");
  fs.mkdirSync(path.dirname(target), { recursive: true });
  const oldDownload = await downloadBoundAsset(oldRelease, target);
  const oldPe = await readPeVersion(target);
  if (versionTriplet(oldPe.fileVersion) !== "0.2.7" || versionTriplet(oldPe.productVersion) !== "0.2.7") throw new Error("installed v0.2.7 PE version is invalid");
  const snapshotProtected = protectedState(scenarioRoot, name);
  const protectedBefore = snapshotProtected();
  const codexHome = path.join(scenarioRoot, "codex-home");
  const workspace = path.join(scenarioRoot, "workspace");
  fs.mkdirSync(codexHome, { recursive: true });
  fs.mkdirSync(workspace, { recursive: true });
  const environment = isolatedEnvironment(scenarioRoot, {
    CODEX_HOME: codexHome,
    CODEX_SQLITE_HOME: codexHome,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port} --remote-allow-origins=*`,
  }, true);
  const child = spawn(target, [], { cwd: workspace, env: environment, windowsHide: true, stdio: "ignore" });
  let initialCdp;
  let restartedCdp;
  let targetLock;
  let restartedObserved = false;
  let failure;
  try {
    const initialTarget = await waitForTauriTarget(port, undefined, 60_000);
    initialCdp = new CdpClient(initialTarget.webSocketDebuggerUrl);
    await initialCdp.connect();
    const initial = await initialCdp.evaluate(`(async()=>{
      const invoke=window.__TAURI_INTERNALS__?.invoke??window.__TAURI__?.core?.invoke??window.__TAURI__?.invoke;
      return {app:await invoke("get_app_status"),update:await invoke("check_for_updates"),readyState:document.readyState,title:document.title,bodyTextLength:document.body?.innerText?.length??0};
    })()`, 60_000);
    if (initial.app.version !== "0.2.7" || !initial.update.updateAvailable || initial.update.latestVersion !== "0.3.0" || initial.readyState !== "complete") throw new Error("v0.2.7 did not observe the public v0.3.0 update");
    if (rollback) targetLock = await startTargetLock(target, path.join(scenarioRoot, "control"));
    const receipt = await initialCdp.evaluate(invokeExpression("install_update"), 12 * 60_000);
    if (receipt.fromVersion !== "0.2.7" || receipt.toVersion !== "0.3.0" || receipt.sha256 !== newRelease.asset.sha256 || receipt.downloadedBytes !== newRelease.asset.bytes || !receipt.restarting) {
      throw new Error("production updater receipt does not match the public asset");
    }
    initialCdp.close();
    initialCdp = undefined;
    const expectedVersion = rollback ? "0.2.7" : "0.3.0";
    const expectedNotice = rollback ? "rolledBack" : "updated";
    await waitForOriginalPidExit(target, child.pid);
    const restarted = await waitForRestartedApp(port, expectedVersion, expectedNotice);
    restartedCdp = restarted.cdp;
    restartedObserved = true;
    if (restarted.state.notice?.status !== expectedNotice || restarted.state.title !== "ChatGPT Switch") throw new Error("restarted application notice does not prove the expected ACK branch");
    const processes = await processRowsForExecutable(target);
    if (processes.length !== 1 || processes[0].pid === child.pid) throw new Error("updater did not produce one distinct same-path application PID");
    const restartedPid = processes[0].pid;
    const targetHash = sha256File(target);
    const expectedHash = rollback ? oldRelease.asset.sha256 : newRelease.asset.sha256;
    if (targetHash !== expectedHash) throw new Error("same-path updater target hash is wrong after ACK");
    if (targetLock) {
      await targetLock.release();
      targetLock = undefined;
    }
    await waitForNoUpdaterResidue(scenarioRoot);
    await restartedCdp.evaluate(invokeExpression("request_app_exit"), 10_000);
    restartedCdp.close();
    restartedCdp = undefined;
    await waitForNoExecutableProcess(target, 30_000);
    await waitForDebugPortClosed(port, 30_000);
    const residualProcesses = await countProcessesInside(scenarioRoot);
    if (residualProcesses.length !== 0) throw new Error("an updater test-owned process remained after graceful close");
    const protectedAfter = snapshotProtected();
    if (JSON.stringify(protectedBefore) !== JSON.stringify(protectedAfter)) throw new Error("protected state changed during update");
    const residues = updaterResidues(scenarioRoot);
    if (residues.staging.length || residues.installArtifacts.length) throw new Error("updater residue reappeared after close");
    return {
      scenario: name,
      verdict: "PASS",
      productionPath: "check_for_updates -> install_update -> helper -> Ready ACK",
      injection: rollback ? "external exact-target read handle with no write/delete sharing" : "none",
      oldAsset: { bytes: oldDownload.bytes, sha256: oldDownload.sha256 },
      initial: { version: initial.app.version, pid: child.pid, ready: true, updateAvailable: true },
      receipt,
      restarted: {
        version: restarted.state.version,
        pid: restartedPid,
        pidChanged: restartedPid !== child.pid,
        sameExecutablePath: true,
        ready: restarted.state.readyState === "complete",
        startupNotice: restarted.state.notice.status,
        acknowledgedBranch: expectedNotice,
      },
      installedTarget: { bytes: fs.statSync(target).size, sha256: targetHash },
      protectedState: { before: protectedBefore, after: protectedAfter, byteExact: true },
      gracefulClose: { command: "request_app_exit", exactTargetProcesses: 0, debugPortClosed: true },
      residuals: { updaterStagingDirectories: 0, installArtifacts: 0, testOwnedProcesses: 0 },
    };
  } catch (error) {
    failure = error;
    throw error;
  } finally {
    if (targetLock) {
      try { await targetLock.release(); } catch {}
    }
    initialCdp?.close();
    if (restartedCdp) {
      try { await restartedCdp.evaluate(invokeExpression("request_app_exit"), 10_000); } catch {}
      restartedCdp.close();
    }
    if (!restartedObserved && failure) {
      // Fail closed without force-killing unrelated processes. Only the exact target is addressed.
      const rows = await processRowsForExecutable(target).catch(() => []);
      if (rows.length === 1) {
        try {
          const fallbackTarget = await waitForTauriTarget(port, undefined, 2_000);
          const fallback = new CdpClient(fallbackTarget.webSocketDebuggerUrl);
          await fallback.connect();
          await fallback.evaluate(invokeExpression("request_app_exit"), 5_000).catch(() => {});
          fallback.close();
        } catch {}
      }
    }
  }
}

export async function runUpdaterE2e(options) {
  if (process.platform !== "win32") throw new Error("updater E2E is Windows-only");
  if (options.dryRun) {
    await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", "$PSVersionTable.PSVersion.ToString()"]);
    return { verdict: "DRY_RUN_PASS", runRootCreated: false, networkUsed: false, executableRun: false, productionSeamAdded: false };
  }
  fs.mkdirSync(options.runRoot, { recursive: false });
  const [oldRelease, newRelease, latest] = await Promise.all([
    resolvePublicRelease(options.repository, OLD_TAG, { expectedSha256: V02_RELEASE_SHA256[OLD_TAG] }),
    resolvePublicRelease(options.repository, NEW_TAG),
    fetchGithubJson(`https://api.github.com/repos/${options.repository}/releases/latest`),
  ]);
  if (latest.tag_name !== NEW_TAG || latest.id !== newRelease.releaseId || latest.draft || latest.prerelease) throw new Error("GitHub Latest is not the exact public v0.3.0 Release");
  const publicAsset = requireInside(options.runRoot, path.join(options.runRoot, "public", NEW_TAG, "codex-switch.exe"), "public asset proof");
  const publicDownload = await downloadBoundAsset(newRelease, publicAsset);
  const publicPe = await readPeVersion(publicAsset);
  if (versionTriplet(publicPe.fileVersion) !== "0.3.0" || versionTriplet(publicPe.productVersion) !== "0.3.0") throw new Error("public v0.3.0 PE version is invalid");

  const scenarioNames = options.mode === "both" ? ["success", "replacement-lock-rollback"] : [options.mode];
  const scenarios = [];
  for (const [index, name] of scenarioNames.entries()) {
    scenarios.push(await runScenario(options.runRoot, name, options.basePort + index, oldRelease, newRelease));
  }
  const payload = {
    gate: "v0.3.0-public-updater-and-rollback",
    verdict: "PASS",
    generatedAt: new Date().toISOString(),
    repository: options.repository,
    latestReleaseVerified: true,
    productionTrustBypass: false,
    productionFailureInjectionSeam: false,
    releases: {
      from: oldRelease,
      to: newRelease,
    },
    publicAssetReadback: {
      path: relativeEvidencePath(options.runRoot, publicAsset),
      bytes: publicDownload.bytes,
      sha256: publicDownload.sha256,
      fileVersion: publicPe.fileVersion,
      productVersion: publicPe.productVersion,
    },
    scenarios,
  };
  assertEvidenceHasNoAbsoluteWindowsPath(payload);
  return writeIntegrityEvidence(options.runRoot, "evidence/updater-e2e.json", payload);
}

async function main() {
  const options = parseUpdaterOptions(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(UPDATER_HELP.trimStart());
    return;
  }
  const result = await runUpdaterE2e(options);
  if (options.dryRun) process.stdout.write(`${JSON.stringify(result)}\n`);
  else process.stdout.write(`EVIDENCE=${relativeEvidencePath(options.runRoot, result.output)}\nSHA256=${result.sha256}\nPAYLOAD_SHA256=${result.payloadSha256}\n`);
}

if (isMain(import.meta.url)) {
  main().catch((error) => {
    process.stderr.write(`v0.3 updater E2E failed: ${error.message}\n`);
    process.exitCode = 1;
  });
}
