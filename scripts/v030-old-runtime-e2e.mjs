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
  assertOnlyFlags,
  closeMainWindowExact,
  copyBoundExecutable,
  downloadBoundAsset,
  invokeExpression,
  isMain,
  isolatedEnvironment,
  normalizeAbsolute,
  parseArgs,
  processRowsForExecutable,
  readPeVersion,
  relativeEvidencePath,
  requireInside,
  requireIgnoredEvidenceRoot,
  resolvePublicRelease,
  resolvePublicTagCommit,
  runCommand,
  sha256File,
  sleep,
  treeDigest,
  versionTriplet,
  waitForNoExecutableProcess,
  waitForDebugPortClosed,
  waitForTauriTarget,
  writeIntegrityEvidence,
} from "./v030-e2e-lib.mjs";

const SCRIPT_DIR = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(SCRIPT_DIR, "..");
const DEFAULT_REPOSITORY = "mingisrookie/codex-switch";
const V026_TAG_COMMIT = "6335fe77a8749a90585969e6ed34d2089023a1b3";
const PACKAGE_NAME = (patch) => `codex-switch-downgrade-0-2-${patch}-downgrade-runtime-v0-2-${patch}`;

export const OLD_RUNTIME_HELP = `
Usage:
  node scripts/v030-old-runtime-e2e.mjs --run-root <absolute-new-directory> [options]

Options:
  --run-root <path>       New final root. Packages are generated here and are never moved.
  --repository <owner/repo>  Public release repository (default: ${DEFAULT_REPOSITORY}).
  --codex-exe <path>      Exact native codex.exe used for list/resume/continue.
  --v026-exe <path>       Exact locally built v0.2.6 EXE bound to its public tag and SHA-256 pin.
  --runtime-source-root <path>  Optional verified cache containing the eight generated package directories.
  --base-port <port>      First WebView2 debugging port (default: OS-selected free port per version).
  --dry-run               Validate local inputs only; do not create files, use network, or run EXEs.
  --help                  Show this help.

The live gate downloads the seven published v0.2.x assets and copies the pinned
local v0.2.6 tag build directly into final packages. It verifies Release/tag/
SHA-256 provenance and PE versions, first verifies native Codex list/resume/turn-start
continuation against immutable packages, then exercises the exact old Switch UI. Evidence is written under <run-root>/evidence.
`;

export function parseOldRuntimeOptions(argv) {
  const { values, flags } = parseArgs(argv, new Set(["run-root", "repository", "codex-exe", "v026-exe", "runtime-source-root", "base-port"]));
  assertOnlyFlags(flags, new Set(["help", "dry-run"]));
  if (flags.has("help")) return { help: true };
  const runRoot = requireIgnoredEvidenceRoot(values.get("run-root"), "run root");
  const repository = values.get("repository") ?? DEFAULT_REPOSITORY;
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository)) throw new Error("repository is invalid");
  const codexExe = values.has("codex-exe") ? normalizeAbsolute(values.get("codex-exe"), "codex executable") : undefined;
  if (codexExe && (!fs.existsSync(codexExe) || !fs.statSync(codexExe).isFile())) throw new Error("codex executable is missing");
  const v026Exe = values.has("v026-exe") ? normalizeAbsolute(values.get("v026-exe"), "v0.2.6 executable") : undefined;
  if (v026Exe && (!fs.existsSync(v026Exe) || !fs.statSync(v026Exe).isFile())) throw new Error("v0.2.6 executable is missing");
  const runtimeSourceRoot = values.has("runtime-source-root") ? normalizeAbsolute(values.get("runtime-source-root"), "runtime source root") : undefined;
  if (runtimeSourceRoot && (!fs.existsSync(runtimeSourceRoot) || !fs.statSync(runtimeSourceRoot).isDirectory())) throw new Error("runtime source root is missing");
  const basePort = values.has("base-port") ? Number(values.get("base-port")) : undefined;
  if (basePort !== undefined && (!Number.isInteger(basePort) || basePort < 10_000 || basePort > 65_000 - 7)) throw new Error("base port is invalid");
  return { help: false, dryRun: flags.has("dry-run"), runRoot, repository, codexExe, v026Exe, runtimeSourceRoot, basePort };
}

async function selectDebugPort(requested) {
  return await new Promise((resolve, reject) => {
    const server = net.createServer();
    server.unref();
    server.once("error", (error) => reject(new Error(`WebView2 debugging port is unavailable: ${error.code ?? error.message}`)));
    server.listen({ host: "127.0.0.1", port: requested ?? 0, exclusive: true }, () => {
      const address = server.address();
      const port = typeof address === "object" && address ? address.port : undefined;
      server.close((error) => error ? reject(error) : resolve(port));
    });
  });
}

function validateFixtureIndex(runRoot) {
  const indexPath = requireInside(runRoot, path.join(runRoot, "fixture-index.json"), "fixture index");
  const index = JSON.parse(fs.readFileSync(indexPath, "utf8"));
  if (index.schemaVersion !== 1 || !/^[0-9a-f-]{36}$/i.test(index.threadId ?? "") || !Array.isArray(index.packages) || index.packages.length !== 8) {
    throw new Error("downgrade fixture index is invalid");
  }
  const packages = [];
  for (let patch = 0; patch <= 7; patch += 1) {
    const expectedVersion = `v0.2.${patch}`;
    const entry = index.packages.find((item) => item.version === expectedVersion);
    const expectedRelative = `packages/${PACKAGE_NAME(patch)}`;
    if (!entry || entry.relativePackagePath !== expectedRelative || !entry.structurallyVerified || entry.logicalSessionCount < 1 || entry.sessionFileCount < 1) {
      throw new Error(`${expectedVersion} fixture binding is invalid`);
    }
    const packageDir = requireInside(runRoot, path.join(runRoot, ...expectedRelative.split("/")), `${expectedVersion} package`);
    if (!fs.statSync(packageDir).isDirectory()) throw new Error(`${expectedVersion} package is missing`);
    for (const relative of ["codex-home", "appdata", "manifest.json", ".codex-switch-downgrade-v1"]) {
      requireInside(packageDir, path.join(packageDir, relative), `${expectedVersion} payload`);
      if (!fs.existsSync(path.join(packageDir, relative))) throw new Error(`${expectedVersion} package payload is incomplete`);
    }
    packages.push({ patch, version: expectedVersion, packageDir, entry });
  }
  return { index, packages };
}

async function closeOldSwitchGracefully({ patch, child, cdp, executable, port, phase = () => {} }) {
  let commandAccepted = false;
  if (patch >= 3 && cdp) {
    try {
      await cdp.evaluate(invokeExpression("request_app_exit"), 10_000);
      commandAccepted = true;
    } catch {}
  }
  phase("close-cdp-start");
  cdp?.close();
  phase("close-cdp-done");
  if (!commandAccepted) {
    phase("wm-close-start");
    await closeMainWindowExact(child.pid, executable);
    phase("wm-close-accepted");
  }
  phase("process-exit-wait-start");
  let exactTestProcessCleanup = false;
  try {
    await waitForNoExecutableProcess(executable, patch < 3 ? 5_000 : 30_000);
  } catch (error) {
    if (patch >= 3) throw error;
    const rows = await processRowsForExecutable(executable);
    if (rows.length !== 1 || rows[0].pid !== child.pid || !child.kill()) {
      throw new Error("legacy exact test process identity could not be proven for cleanup");
    }
    exactTestProcessCleanup = true;
    phase("exact-test-process-cleanup", { pid: child.pid });
    await waitForNoExecutableProcess(executable, 30_000);
  }
  phase("process-exit-verified");
  phase("debug-port-close-wait-start");
  await waitForDebugPortClosed(port, 30_000);
  phase("debug-port-close-verified");
  return {
    method: commandAccepted ? "request_app_exit" : "WM_CLOSE",
    processExited: true,
    debugPortClosed: true,
    exactTestProcessCleanup,
  };
}

async function primeOldSwitchWebViewProfile(packageDir, executable, workspace, environment, port) {
  const localState = path.join(packageDir, "codex-switch.exe.WebView2", "EBWebView", "Local State");
  if (fs.existsSync(localState)) return false;
  const child = spawn(executable, [], {
    cwd: workspace,
    env: environment,
    windowsHide: true,
    stdio: "ignore",
  });
  let primaryError;
  try {
    const deadline = Date.now() + 60_000;
    while (!fs.existsSync(localState) && Date.now() < deadline) await sleep(250);
    if (!fs.existsSync(localState)) throw new Error("WebView2 profile bootstrap did not create Local State");
    await sleep(2_000);
    await closeMainWindowExact(child.pid, executable);
    await waitForNoExecutableProcess(executable, 90_000);
    await waitForDebugPortClosed(port, 30_000);
    return true;
  } catch (error) {
    primaryError = error;
    throw error;
  } finally {
    if (await processRowsForExecutable(executable).then((rows) => rows.length > 0).catch(() => false)) {
      try {
        await closeMainWindowExact(child.pid, executable);
        await waitForNoExecutableProcess(executable, 90_000);
      } catch (cleanupError) {
        if (!primaryError) throw cleanupError;
      }
    }
  }
}

export async function exerciseOldSwitchUi(runRoot, fixture, port) {
  const { patch, version, packageDir } = fixture;
  const phasePath = requireInside(runRoot, path.join(runRoot, "evidence", "old-runtime-ui-phases.jsonl"), "UI phase trace");
  fs.mkdirSync(path.dirname(phasePath), { recursive: true });
  const phase = (name, details = {}) => fs.appendFileSync(
    phasePath,
    `${JSON.stringify({ observedAt: new Date().toISOString(), version, phase: name, ...details })}\n`,
    "utf8",
  );
  const executable = requireInside(packageDir, path.join(packageDir, "codex-switch.exe"), `${version} executable`);
  const codexHome = requireInside(packageDir, path.join(packageDir, "codex-home"), `${version} CODEX_HOME`);
  const workspace = requireInside(packageDir, path.join(packageDir, "workspace"), `${version} workspace`);
  fs.mkdirSync(workspace, { recursive: true });
  const environment = isolatedEnvironment(packageDir, {
    CODEX_HOME: codexHome,
    CODEX_SQLITE_HOME: codexHome,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port} --remote-allow-origins=*`,
  }, false);
  const webViewProfilePrimed = await primeOldSwitchWebViewProfile(
    packageDir,
    executable,
    workspace,
    environment,
    port,
  );
  phase("profile-ready", { webViewProfilePrimed, port });
  const child = spawn(executable, [], {
    cwd: workspace,
    env: environment,
    windowsHide: true,
    stdio: "ignore",
  });
  let childExit;
  let childSpawnError;
  child.once("exit", (code, signal) => { childExit = { code, signal }; });
  child.once("error", (error) => { childSpawnError = String(error); });
  phase("spawned", { pid: child.pid });
  let cdp;
  let graceful;
  let primaryError;
  try {
    let target;
    try {
      // A newly copied PE and a fresh WebView2 profile can spend more than one
      // minute in Windows reputation scanning on the first isolated launch.
      target = await waitForTauriTarget(port, undefined, 180_000);
    } catch (error) {
      throw new Error(`${error.message}; childExit=${JSON.stringify(childExit ?? null)}; childSpawnError=${childSpawnError ?? "none"}`);
    }
    phase("cdp-target-ready");
    cdp = new CdpClient(target.webSocketDebuggerUrl);
    await cdp.connect();
    phase("cdp-connected");
    let rendered;
    const renderDeadline = Date.now() + 60_000;
    while (Date.now() < renderDeadline) {
      rendered = await cdp.evaluate(`(()=>({
        title:document.title,
        readyState:document.readyState,
        bodyTextLength:document.body?.innerText?.length??0,
        invokeReady:typeof(window.__TAURI_INTERNALS__?.invoke??window.__TAURI__?.core?.invoke??window.__TAURI__?.invoke)==="function"
      }))()`);
      if (rendered.title === "ChatGPT Switch" && rendered.readyState === "complete" && rendered.bodyTextLength >= 50 && rendered.invokeReady) break;
      await sleep(250);
    }
    if (rendered?.title !== "ChatGPT Switch" || rendered.readyState !== "complete" || rendered.bodyTextLength < 50 || !rendered.invokeReady) {
      throw new Error(`${version} UI did not render: ${JSON.stringify(rendered ?? null)}`);
    }
    phase("ui-rendered", { bodyTextLength: rendered.bodyTextLength });
    const status = await cdp.evaluate(`(async()=>{
      const invoke=window.__TAURI_INTERNALS__?.invoke??window.__TAURI__?.core?.invoke??window.__TAURI__?.invoke;
      const call=async(command)=>{try{return {ok:true,value:await invoke(command)}}catch(error){return {ok:false,error:String(error)}}};
      return {title:document.title,readyState:document.readyState,bodyTextLength:document.body?.innerText?.length??0,app:await call("get_app_status"),sessions:await call("scan_sessions"),managed:await call("scan_managed_sessions")};
    })()`);
    const actualHome = String(status.app?.value?.codexHome ?? "");
    if (status.title !== "ChatGPT Switch" || status.readyState !== "complete" || status.bodyTextLength < 50) throw new Error(`${version} UI did not render`);
    if (!status.app?.ok || status.app.value.version !== `0.2.${patch}` || path.resolve(actualHome).toLowerCase() !== path.resolve(codexHome).toLowerCase()) throw new Error(`${version} app status is not bound to its package`);
    if (!status.sessions?.ok || status.sessions.value.threadCount < 1 || status.sessions.value.sessionJsonlCount < 1) throw new Error(`${version} session inventory is incomplete`);
    if (!status.managed?.ok || status.managed.value.totalCount < 1) throw new Error(`${version} managed session inventory is incomplete`);
    phase("runtime-status-verified");

    const navigated = await cdp.evaluate(`(()=>{const button=Array.from(document.querySelectorAll("button")).find(item=>item.textContent?.trim()==="会话");if(!button)return false;button.click();return true})()`);
    if (!navigated) throw new Error(`${version} session navigation is missing`);
    let sessionRows = 0;
    let titleFound = false;
    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      const page = await cdp.evaluate(`(()=>({rows:document.querySelectorAll(".session-row").length,text:document.body?.innerText??""}))()`);
      sessionRows = page.rows;
      titleFound = /downgrade fixture/i.test(page.text);
      if (sessionRows >= 1 && titleFound) break;
      await sleep(250);
    }
    if (sessionRows < 1 || !titleFound) throw new Error(`${version} session page did not render the fixture`);
    phase("session-page-verified", { sessionRows });
    const screenshot = await cdp.call("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
    const screenshotPath = requireInside(runRoot, path.join(runRoot, "evidence", "screenshots", `${version}.png`), "screenshot");
    fs.mkdirSync(path.dirname(screenshotPath), { recursive: true });
    fs.writeFileSync(screenshotPath, Buffer.from(screenshot.data, "base64"));
    phase("screenshot-written");
    graceful = await closeOldSwitchGracefully({ patch, child, cdp, executable, port, phase });
    phase("graceful-close-verified", { method: graceful.method });
    cdp = undefined;
    if ((await processRowsForExecutable(executable)).length !== 0) throw new Error(`${version} left an exact executable process behind`);
    return {
      version,
      appVersion: status.app.value.version,
      codexHomeBoundToFinalPackage: true,
      threadCount: status.sessions.value.threadCount,
      sessionJsonlCount: status.sessions.value.sessionJsonlCount,
      managedSessionCount: status.managed.value.totalCount,
      webViewProfilePrimed,
      renderedSessionRows: sessionRows,
      renderedSessionTitleFound: titleFound,
      gracefulClose: graceful,
      screenshot: {
        path: relativeEvidencePath(runRoot, screenshotPath),
        bytes: fs.statSync(screenshotPath).size,
        sha256: sha256File(screenshotPath),
      },
    };
  } catch (error) {
    primaryError = error;
    throw error;
  } finally {
    if (!graceful) {
      try {
        await closeOldSwitchGracefully({ patch, child, cdp, executable, port, phase });
      } catch (cleanupError) {
        if (!primaryError) throw cleanupError;
      }
    }
  }
}

async function generateFixtures(runRoot, codexExe) {
  const environment = { ...process.env, CODEX_SWITCH_DOWNGRADE_FIXTURE_ROOT: runRoot };
  if (codexExe) environment.CODEX_SWITCH_CODEX_RUNTIME_EXE = codexExe;
  await runCommand("cargo", [
    "test", "--locked", "--manifest-path", "src-tauri/Cargo.toml",
    "--test", "downgrade_old_runtime_contract",
    "exports_every_supported_v02_runtime_fixture", "--", "--ignored", "--exact",
  ], { cwd: REPO_ROOT, env: environment, timeoutMs: 20 * 60_000 });
}

async function runNativeContinuation(options, outputPath) {
  const pythonArgs = [
    path.join(SCRIPT_DIR, "v030-old-runtime-e2e-native.py"),
    "--root", options.runRoot,
    "--output", outputPath,
  ];
  if (options.codexExe) pythonArgs.push("--codex-exe", options.codexExe);
  await runCommand("python", pythonArgs, { cwd: REPO_ROOT, timeoutMs: 20 * 60_000 });
  const result = JSON.parse(fs.readFileSync(outputPath, "utf8"));
  if (result.schemaVersion !== 1 || result.verdict !== "PASS" || result.versions?.length !== 8) throw new Error("native old-runtime verification did not pass all versions");
  return result;
}

export async function runOldRuntimeE2e(options) {
  if (process.platform !== "win32") throw new Error("old-runtime E2E is Windows-only");
  if (options.dryRun) {
    for (const required of [
      "src-tauri/Cargo.toml",
      "src-tauri/tests/downgrade_old_runtime_contract.rs",
      "scripts/v030-old-runtime-e2e-native.py",
    ]) {
      if (!fs.existsSync(path.join(REPO_ROOT, required))) throw new Error(`required input is missing: ${required}`);
    }
    await runCommand("cargo", ["--version"], { cwd: REPO_ROOT });
    await runCommand("python", ["--version"], { cwd: REPO_ROOT });
    return { verdict: "DRY_RUN_PASS", runRootCreated: false, networkUsed: false, executableRun: false };
  }
  if (!options.v026Exe && !options.runtimeSourceRoot) throw new Error("live old-runtime E2E requires --v026-exe or --runtime-source-root because v0.2.6 intentionally has no GitHub Release");

  await generateFixtures(options.runRoot, options.codexExe);
  const { index, packages } = validateFixtureIndex(options.runRoot);
  const inputBefore = treeDigest(path.join(options.runRoot, "input"));
  const nativeOutput = requireInside(options.runRoot, path.join(options.runRoot, "evidence", "native-runtime.json"), "native evidence");
  fs.mkdirSync(path.dirname(nativeOutput), { recursive: true });
  const native = await runNativeContinuation(options, nativeOutput);
  const bindings = [];
  for (const fixture of packages) {
    const executable = requireInside(fixture.packageDir, path.join(fixture.packageDir, "codex-switch.exe"), `${fixture.version} executable`);
    const cachedExecutable = options.runtimeSourceRoot
      ? requireInside(options.runtimeSourceRoot, path.join(options.runtimeSourceRoot, PACKAGE_NAME(fixture.patch), "codex-switch.exe"), `${fixture.version} cached executable`)
      : undefined;
    let binding;
    if (fixture.version === "v0.2.6") {
      const tagCommit = await resolvePublicTagCommit(options.repository, fixture.version);
      if (tagCommit !== V026_TAG_COMMIT) throw new Error("v0.2.6 public tag commit changed from its local-build pin");
      const copy = copyBoundExecutable(cachedExecutable ?? options.v026Exe, executable, V02_RELEASE_SHA256[fixture.version]);
      binding = {
        provenance: cachedExecutable ? "verifiedLocalTagBuildCache" : "localTagBuild",
        tagCommit,
        bytes: copy.bytes,
        sha256: copy.sha256,
      };
    } else {
      const release = await resolvePublicRelease(options.repository, fixture.version, {
        expectedSha256: V02_RELEASE_SHA256[fixture.version],
      });
      const download = cachedExecutable
        ? copyBoundExecutable(cachedExecutable, executable, V02_RELEASE_SHA256[fixture.version])
        : await downloadBoundAsset(release, executable);
      binding = {
        provenance: cachedExecutable ? "verifiedPublicReleaseCache" : "publicRelease",
        tagCommit: release.tagCommit,
        releaseId: release.releaseId,
        releaseUrl: release.releaseUrl,
        publishedAt: release.publishedAt,
        assetId: release.asset.assetId,
        assetUrl: release.asset.downloadUrl,
        bytes: download.bytes,
        sha256: download.sha256,
      };
    }
    const pe = await readPeVersion(executable);
    const expected = fixture.version.slice(1);
    if (versionTriplet(pe.fileVersion) !== expected || versionTriplet(pe.productVersion) !== expected) throw new Error(`${fixture.version} PE version is invalid`);
    bindings.push({
      version: fixture.version,
      package: fixture.entry.relativePackagePath,
      ...binding,
      fileVersion: pe.fileVersion,
      productVersion: pe.productVersion,
    });
  }

  const ui = [];
  for (const fixture of packages) {
    const requestedPort = options.basePort === undefined ? undefined : options.basePort + fixture.patch;
    const port = await selectDebugPort(requestedPort);
    try {
      ui.push(await exerciseOldSwitchUi(options.runRoot, fixture, port));
    } catch (error) {
      throw new Error(`${fixture.version} old UI failed on debug port ${port}: ${error.message}`);
    }
  }
  const inputAfter = treeDigest(path.join(options.runRoot, "input"));
  if (JSON.stringify(inputBefore) !== JSON.stringify(inputAfter)) throw new Error("fixture source input changed during old-runtime verification");
  for (const fixture of packages) {
    const executable = path.join(fixture.packageDir, "codex-switch.exe");
    if ((await processRowsForExecutable(executable)).length !== 0) throw new Error(`${fixture.version} test process remains`);
  }

  const payload = {
    gate: "v0.3.0-exact-old-runtime-matrix",
    verdict: "PASS",
    generatedAt: new Date().toISOString(),
    repository: options.repository,
    packageContract: "generated directly at final absolute paths; packages were never copied or moved",
    outboundPolicy: "old Switch UI outbound traffic blocked; native continuation uses loopback Responses only",
    threadId: index.threadId,
    inputSentinel: { before: inputBefore, after: inputAfter, unchanged: true },
    executableBindings: bindings,
    oldSwitchUi: ui,
    nativeRuntime: {
      path: relativeEvidencePath(options.runRoot, nativeOutput),
      sha256: sha256File(nativeOutput),
      runtime: native.runtime,
      versions: native.versions,
    },
    residuals: { exactOldSwitchProcesses: 0 },
  };
  assertEvidenceHasNoAbsoluteWindowsPath(payload);
  return writeIntegrityEvidence(options.runRoot, "evidence/old-runtime-e2e.json", payload);
}

async function main() {
  const options = parseOldRuntimeOptions(process.argv.slice(2));
  if (options.help) {
    process.stdout.write(OLD_RUNTIME_HELP.trimStart());
    return;
  }
  const result = await runOldRuntimeE2e(options);
  if (options.dryRun) process.stdout.write(`${JSON.stringify(result)}\n`);
  else process.stdout.write(`EVIDENCE=${relativeEvidencePath(options.runRoot, result.output)}\nSHA256=${result.sha256}\nPAYLOAD_SHA256=${result.payloadSha256}\n`);
}

if (isMain(import.meta.url)) {
  main().catch((error) => {
    process.stderr.write(`v0.3 old-runtime E2E failed: ${error.message}\n`);
    process.exitCode = 1;
  });
}
