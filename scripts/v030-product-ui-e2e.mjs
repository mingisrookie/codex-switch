#!/usr/bin/env node

import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import readline from "node:readline";
import { spawn } from "node:child_process";
import {
  CdpClient,
  assertEvidenceHasNoAbsoluteWindowsPath,
  closeMainWindowExact,
  invokeExpression,
  isolatedEnvironment,
  processRowsForExecutable,
  relativeEvidencePath,
  requireIgnoredEvidenceRoot,
  requireInside,
  sha256File,
  sleep,
  waitForDebugPortClosed,
  waitForNoExecutableProcess,
  waitForTauriTarget,
  writeIntegrityEvidence,
} from "./v030-e2e-lib.mjs";

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

async function hasListener(port) {
  return await new Promise((resolve) => {
    const socket = net.createConnection({ host: "127.0.0.1", port });
    const done = (value) => { socket.destroy(); resolve(value); };
    socket.setTimeout(500, () => done(false));
    socket.once("connect", () => done(true));
    socket.once("error", () => done(false));
  });
}

async function primeWebViewProfile(root, executable, workspace, environment, port) {
  const localState = path.join(root, "install", "codex-switch.exe.WebView2", "EBWebView", "Local State");
  if (fs.existsSync(localState)) return false;
  const child = spawn(executable, [], { cwd: workspace, env: environment, windowsHide: true, stdio: "ignore" });
  try {
    const deadline = Date.now() + 60_000;
    while (!fs.existsSync(localState) && Date.now() < deadline) await sleep(250);
    if (!fs.existsSync(localState)) throw new Error("WebView2 profile bootstrap did not create Local State");
    await sleep(2_000);
    await closeMainWindowExact(child.pid, executable);
    await waitForNoExecutableProcess(executable, 90_000);
    await waitForDebugPortClosed(port, 30_000);
    return true;
  } finally {
    if ((await processRowsForExecutable(executable)).length > 0) {
      await closeMainWindowExact(child.pid, executable).catch(() => {});
      await waitForNoExecutableProcess(executable, 30_000).catch(() => {});
    }
  }
}

async function initializeNativeCodexState(codexExe, codexHome, workspace, environment) {
  const child = spawn(codexExe, ["app-server", "--stdio", "--disable", "plugins"], {
    cwd: workspace,
    env: { ...environment, CODEX_HOME: codexHome, CODEX_SQLITE_HOME: codexHome },
    windowsHide: true,
    stdio: ["pipe", "pipe", "ignore"],
  });
  const lines = readline.createInterface({ input: child.stdout });
  const acknowledged = new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("native Codex schema initialization timed out")), 30_000);
    lines.on("line", (line) => {
      try {
        const message = JSON.parse(line);
        if (message.id === 1) {
          clearTimeout(timer);
          resolve(Boolean(message.result));
        }
      } catch {}
    });
    child.once("error", (error) => { clearTimeout(timer); reject(error); });
  });
  child.stdin.write(`${JSON.stringify({ id: 1, method: "initialize", params: { clientInfo: { name: "codex-switch-product-ui", version: "0.3.0" }, capabilities: { experimentalApi: false } } })}\n`);
  const accepted = await acknowledged;
  if (!accepted) throw new Error("native Codex rejected schema initialization");
  child.stdin.write(`${JSON.stringify({ method: "initialized", params: {} })}\n`);
  child.stdin.end();
  const status = await new Promise((resolve, reject) => {
    child.once("exit", (code, signal) => resolve({ code, signal }));
    child.once("error", reject);
  });
  lines.close();
  if (status.code !== 0 || !fs.statSync(path.join(codexHome, "state_5.sqlite")).isFile()) throw new Error("native Codex did not create the product UI state database");
}

function options(argv) {
  const values = new Map();
  let dryRun = false;
  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];
    if (token === "--dry-run") { dryRun = true; continue; }
    if (token === "--help") return { help: true };
    if (!new Set(["--run-root", "--executable", "--codex-exe"]).has(token) || !argv[index + 1]) throw new Error(`invalid option ${token}`);
    values.set(token.slice(2), argv[++index]);
  }
  const executable = path.resolve(values.get("executable") ?? "");
  if (!path.isAbsolute(values.get("executable") ?? "") || !fs.statSync(executable).isFile()) throw new Error("executable must be an existing absolute file");
  const codexExe = values.has("codex-exe") ? path.resolve(values.get("codex-exe")) : undefined;
  if (codexExe && !fs.statSync(codexExe).isFile()) throw new Error("codex executable must be an existing file");
  return { help: false, dryRun, executable, codexExe, runRoot: requireIgnoredEvidenceRoot(values.get("run-root"), "run root") };
}

async function main() {
  const parsed = options(process.argv.slice(2));
  if (parsed.help) {
    process.stdout.write("Usage: node scripts/v030-product-ui-e2e.mjs --run-root <new-absolute-path> --executable <packed-exe> --codex-exe <native-codex.exe> [--dry-run]\n");
    return;
  }
  if (parsed.dryRun) {
    process.stdout.write(JSON.stringify({ verdict: "DRY_RUN_PASS", runRootCreated: false }) + "\n");
    return;
  }
  if (await hasListener(1420)) throw new Error("Vite port 1420 must have no listener during the production UI gate");
  fs.mkdirSync(parsed.runRoot);
  const executable = requireInside(parsed.runRoot, path.join(parsed.runRoot, "install", "codex-switch.exe"), "test executable");
  fs.mkdirSync(path.dirname(executable));
  fs.copyFileSync(parsed.executable, executable, fs.constants.COPYFILE_EXCL);
  if (sha256File(executable) !== sha256File(parsed.executable)) throw new Error("test executable copy changed the packed asset");
  const codexHome = requireInside(parsed.runRoot, path.join(parsed.runRoot, "codex-home"), "CODEX_HOME");
  const workspace = requireInside(parsed.runRoot, path.join(parsed.runRoot, "workspace"), "workspace");
  fs.mkdirSync(codexHome);
  fs.mkdirSync(workspace);
  const port = await freePort();
  const environment = isolatedEnvironment(parsed.runRoot, {
    CODEX_HOME: codexHome,
    CODEX_SQLITE_HOME: codexHome,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port} --remote-allow-origins=*`,
  }, false);
  await initializeNativeCodexState(parsed.codexExe ?? "codex.exe", codexHome, workspace, environment);
  fs.writeFileSync(path.join(codexHome, "auth.json"), JSON.stringify({ auth_mode: "chatgpt", tokens: { access_token: "fixture-token" } }));
  fs.writeFileSync(path.join(codexHome, "config.toml"), 'model_provider = "openai"\n');
  const webViewProfilePrimed = await primeWebViewProfile(parsed.runRoot, executable, workspace, environment, port);
  const child = spawn(executable, [], { cwd: workspace, env: environment, windowsHide: true, stdio: "ignore" });
  let cdp;
  let exited = false;
  child.once("exit", () => { exited = true; });
  try {
    const target = await waitForTauriTarget(port, undefined, 180_000);
    cdp = new CdpClient(target.webSocketDebuggerUrl);
    await cdp.connect();
    const runtimeErrors = [];
    cdp.on("Runtime.exceptionThrown", (event) => runtimeErrors.push({ kind: "exception", text: event.exceptionDetails?.text ?? "exception" }));
    cdp.on("Runtime.consoleAPICalled", (event) => {
      if (["error", "assert"].includes(event.type)) runtimeErrors.push({ kind: "console", type: event.type });
    });
    await cdp.call("Runtime.enable");
    await cdp.call("Page.enable");
    await cdp.call("Page.navigate", { url: "http://tauri.localhost/" });
    let rendered;
    const deadline = Date.now() + 60_000;
    while (Date.now() < deadline) {
      rendered = await cdp.evaluate(`(()=>({title:document.title,readyState:document.readyState,text:document.body?.innerText?.length??0,invoke:typeof(window.__TAURI_INTERNALS__?.invoke??window.__TAURI__?.core?.invoke??window.__TAURI__?.invoke)==="function"}))()`);
      if (rendered.title === "ChatGPT Switch" && rendered.readyState === "complete" && rendered.text > 100 && rendered.invoke) break;
      await sleep(250);
    }
    if (rendered?.title !== "ChatGPT Switch" || rendered.readyState !== "complete" || rendered.text <= 100 || !rendered.invoke) {
      throw new Error(`production UI did not render: ${JSON.stringify({ ...rendered, runtimeErrorCount: runtimeErrors.length })}`);
    }
    const visibleStartupText = await cdp.evaluate(`document.body?.innerText??""`);
    assertEvidenceHasNoAbsoluteWindowsPath(visibleStartupText);
    const startupAlerts = await cdp.evaluate(`Array.from(document.querySelectorAll('[role="alert"]')).filter(node=>{const style=getComputedStyle(node);const r=node.getBoundingClientRect();return style.display!=="none"&&style.visibility!=="hidden"&&r.width>0&&r.height>0}).map(node=>node.textContent?.trim()??"")`);
    if (startupAlerts.length > 0) throw new Error(`production UI rendered visible startup alerts: ${startupAlerts.length}`);
    if (runtimeErrors.length > 0) throw new Error(`production UI emitted runtime errors: ${runtimeErrors.length}`);
    const status = await cdp.evaluate(invokeExpression("get_app_status"));
    if (status.version !== "0.3.0" || path.resolve(status.codexHome).toLowerCase() !== codexHome.toLowerCase()) throw new Error("runtime status is not bound to the isolated v0.3.0 process");
    if (await hasListener(1420)) throw new Error("production UI opened with a Vite listener present");

    await cdp.call("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
    const viewports = [];
    for (const [width, height] of [[1200, 820], [900, 640], [390, 844]]) {
      await cdp.call("Emulation.setDeviceMetricsOverride", { width, height, screenWidth: width, screenHeight: height, deviceScaleFactor: 1, mobile: false });
      await sleep(150);
      const hit = await cdp.evaluate(`(()=>{const button=Array.from(document.querySelectorAll("button")).find(item=>item.textContent?.trim()==="存储");if(!button)return {found:false};const r=button.getBoundingClientRect();const top=document.elementFromPoint(r.left+r.width/2,r.top+r.height/2);button.click();return {found:true,hit:top===button||button.contains(top),innerWidth,innerHeight,scrollWidth:document.documentElement.scrollWidth}})()`);
      if (!hit.found || !hit.hit || hit.innerWidth !== width || hit.innerHeight !== height || hit.scrollWidth > width) throw new Error(`viewport ${width}x${height} failed navigation or hit-test`);
      let storageVisible = false;
      const storageDeadline = Date.now() + 10_000;
      while (!storageVisible && Date.now() < storageDeadline) {
        storageVisible = await cdp.evaluate(`(()=>document.querySelector('button[aria-current="page"]')?.textContent?.trim()==="存储"&&Boolean(document.querySelector('[aria-label="会话存储管理"]:not([hidden])')))()`);
        if (!storageVisible) await sleep(100);
      }
      if (!storageVisible) throw new Error(`viewport ${width}x${height} did not render the storage page`);
      let storageState;
      const stateDeadline = Date.now() + 15_000;
      while (Date.now() < stateDeadline) {
        storageState = await cdp.evaluate(`(()=>{const metric=Array.from(document.querySelectorAll('.storage-metric')).find(node=>node.querySelector('span')?.textContent?.trim()==='存储状态');return metric?.querySelector('strong')?.textContent?.trim()??null})()`);
        if (storageState && storageState !== "读取中") break;
        await sleep(100);
      }
      if (!storageState || storageState === "读取中") throw new Error(`viewport ${width}x${height} storage control state did not settle`);
      const quietDeadline = Date.now() + 750;
      while (Date.now() < quietDeadline) {
        const visibleText = await cdp.evaluate(`document.body?.innerText??""`);
        assertEvidenceHasNoAbsoluteWindowsPath(visibleText);
        const visibleAlerts = await cdp.evaluate(`Array.from(document.querySelectorAll('[role="alert"]')).filter(node=>{const style=getComputedStyle(node);const r=node.getBoundingClientRect();return style.display!=="none"&&style.visibility!=="hidden"&&r.width>0&&r.height>0}).length`);
        if (visibleAlerts > 0 || runtimeErrors.length > 0) throw new Error(`viewport ${width}x${height} contains alerts or runtime errors`);
        await sleep(100);
      }
      const screenshot = await cdp.call("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
      const screenshotPath = requireInside(parsed.runRoot, path.join(parsed.runRoot, "screenshots", `${width}x${height}.png`), "screenshot");
      fs.mkdirSync(path.dirname(screenshotPath), { recursive: true });
      fs.writeFileSync(screenshotPath, Buffer.from(screenshot.data, "base64"));
      viewports.push({ width, height, navigationHitTest: true, noHorizontalOverflow: true, storageVisible: true, storageState, quietWindowMs: 750, screenshot: { path: relativeEvidencePath(parsed.runRoot, screenshotPath), bytes: fs.statSync(screenshotPath).size, sha256: sha256File(screenshotPath) } });
    }
    const reducedMotion = await cdp.evaluate(`matchMedia('(prefers-reduced-motion: reduce)').matches`);
    if (!reducedMotion) throw new Error("reduced-motion emulation was not active");
    await cdp.evaluate(invokeExpression("request_app_exit"), 10_000);
    cdp.close();
    cdp = undefined;
    await waitForNoExecutableProcess(executable, 90_000);
    await waitForDebugPortClosed(port, 30_000);

    const diagnosticsRoot = path.join(parsed.runRoot, "appdata", "codex-switch", "logs", "diagnostics");
    const events = fs.existsSync(diagnosticsRoot)
      ? fs.readdirSync(diagnosticsRoot).filter((name) => /^events-.*\.jsonl$/.test(name)).flatMap((name) => fs.readFileSync(path.join(diagnosticsRoot, name), "utf8").trim().split(/\r?\n/).filter(Boolean).map((line) => JSON.parse(line).eventKind))
      : [];
    for (const expected of ["sessionStarted", "appReady", "exitRequested", "sessionEnded"]) if (!events.includes(expected)) throw new Error(`lifecycle evidence omitted ${expected}`);
    const payload = {
      gate: "v0.3.0-packed-production-ui",
      verdict: "PASS",
      observedAt: new Date().toISOString(),
      executable: { bytes: fs.statSync(executable).size, sha256: sha256File(executable), version: status.version, byteExactReleaseCopy: true },
      customProtocol: target.url,
      viteListenerAbsent: true,
      isolatedRuntime: true,
      webViewProfilePrimed,
      reducedMotion: true,
      runtimeErrors: [],
      visibleAlerts: 0,
      lifecycle: ["sessionStarted", "appReady", "exitRequested", "sessionEnded"],
      viewports,
      residuals: { exactExecutableProcesses: (await processRowsForExecutable(executable)).length, debugPortClosed: true },
    };
    assertEvidenceHasNoAbsoluteWindowsPath(payload);
    const evidence = writeIntegrityEvidence(parsed.runRoot, "product-ui-e2e.json", payload);
    process.stdout.write(`EVIDENCE=${relativeEvidencePath(parsed.runRoot, evidence.output)}\nSHA256=${evidence.sha256}\n`);
  } finally {
    cdp?.close();
    if (!exited && (await processRowsForExecutable(executable)).length > 0) {
      try { child.kill(); } catch {}
      await waitForNoExecutableProcess(executable, 30_000).catch(() => {});
    }
  }
}

main().catch((error) => { process.stderr.write(`${error.stack ?? error}\n`); process.exitCode = 1; });
