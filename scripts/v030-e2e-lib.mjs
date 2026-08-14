import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { spawn, spawnSync } from "node:child_process";
import { pathToFileURL } from "node:url";

const GITHUB_API = "https://api.github.com";
const ALLOWED_DOWNLOAD_HOSTS = new Set([
  "github.com",
  "release-assets.githubusercontent.com",
  "objects.githubusercontent.com",
]);

export const V02_RELEASE_SHA256 = Object.freeze({
  "v0.2.0": "42012a1baa73c6f573c5cec047ce3bad0740ddeaf74ff234f80f2d510498a65a",
  "v0.2.1": "8f6ea219a53bb3395f039327a3cd3827b53ee67b8daf4b130e60235940a3020c",
  "v0.2.2": "41d08657b96162617b25b760dd7d8a89527617f1908a57c22a8f46ecd7df4c97",
  "v0.2.3": "0dc884f1ab9a3b4ca0cef3aa2ddc3cf6d36f4fd966bb6396922784d7e06e9ef9",
  "v0.2.4": "cf558eba8a8883714b13aafc4f267fd50cc2af3301d4905eb24843c9258e0992",
  "v0.2.5": "33ae580105c2b947a1487f7af4b337002e4a37fb205c6910eaebf558d65a4ee0",
  "v0.2.6": "f9fff5e00e57c283f67c82a7c586fc9ef201337fa9590fba8698b62c2b79dc49",
  "v0.2.7": "e8905135eab0c5d76117baa25770371e3d8bfdaf07495805463c96a823a5b78e",
});

export function isMain(importMetaUrl, argv = process.argv) {
  return Boolean(argv[1]) && pathToFileURL(path.resolve(argv[1])).href === importMetaUrl;
}

export function parseArgs(argv, valueNames = new Set()) {
  const values = new Map();
  const flags = new Set();
  for (let index = 0; index < argv.length; index += 1) {
    const token = argv[index];
    if (!token.startsWith("--")) {
      throw new Error(`unexpected positional argument: ${token}`);
    }
    const name = token.slice(2);
    if (valueNames.has(name)) {
      const value = argv[index + 1];
      if (!value || value.startsWith("--")) {
        throw new Error(`--${name} requires a value`);
      }
      if (values.has(name)) {
        throw new Error(`--${name} may be specified only once`);
      }
      values.set(name, value);
      index += 1;
    } else {
      if (flags.has(name)) {
        throw new Error(`--${name} may be specified only once`);
      }
      flags.add(name);
    }
  }
  return { values, flags };
}

export function assertOnlyFlags(flags, allowed) {
  for (const flag of flags) {
    if (!allowed.has(flag)) {
      throw new Error(`unknown option: --${flag}`);
    }
  }
}

export function normalizeAbsolute(input, label) {
  if (!input || !path.isAbsolute(input)) {
    throw new Error(`${label} must be an absolute path`);
  }
  const resolved = path.resolve(input);
  if (resolved !== input && resolved.toLowerCase() !== input.toLowerCase()) {
    throw new Error(`${label} must not contain dot segments`);
  }
  return resolved;
}

export function requireNewRoot(input, label = "run root") {
  const root = normalizeAbsolute(input, label);
  if (fs.existsSync(root)) {
    throw new Error(`${label} must not already exist`);
  }
  const parent = path.dirname(root);
  if (!fs.existsSync(parent) || !fs.statSync(parent).isDirectory()) {
    throw new Error(`${label} parent must be an existing directory`);
  }
  return root;
}

export function requireIgnoredEvidenceRoot(input, label = "run root") {
  const root = requireNewRoot(input, label);
  const parent = path.dirname(root);
  const probe = spawnSync("git", ["-C", parent, "rev-parse", "--show-toplevel"], {
    encoding: "utf8",
    windowsHide: true,
  });
  if (probe.status !== 0) return root;
  const gitRoot = path.resolve(probe.stdout.trim());
  if (!isPathInside(gitRoot, root)) throw new Error(`${label} Git boundary is invalid`);
  const relative = path.relative(gitRoot, root).split(path.sep).join("/");
  const ignored = spawnSync("git", ["-C", gitRoot, "check-ignore", "--no-index", "--quiet", "--", relative], {
    windowsHide: true,
  });
  if (ignored.status !== 0) throw new Error(`${label} is inside Git but is not ignored`);
  return root;
}

export function isPathInside(root, candidate, allowRoot = false) {
  const normalizedRoot = path.resolve(root);
  const normalizedCandidate = path.resolve(candidate);
  const relative = path.relative(normalizedRoot, normalizedCandidate);
  if (relative === "") return allowRoot;
  return !relative.startsWith("..") && !path.isAbsolute(relative);
}

export function requireInside(root, candidate, label, allowRoot = false) {
  if (!isPathInside(root, candidate, allowRoot)) {
    throw new Error(`${label} escaped the test-owned run root`);
  }
  return path.resolve(candidate);
}

export function relativeEvidencePath(root, candidate) {
  requireInside(root, candidate, "evidence path", false);
  return path.relative(root, candidate).split(path.sep).join("/");
}

export function sha256Bytes(value) {
  return crypto.createHash("sha256").update(value).digest("hex");
}

export function sha256File(file) {
  const handle = fs.openSync(file, "r");
  const hash = crypto.createHash("sha256");
  const buffer = Buffer.allocUnsafe(1024 * 1024);
  try {
    for (;;) {
      const count = fs.readSync(handle, buffer, 0, buffer.length, null);
      if (count === 0) break;
      hash.update(buffer.subarray(0, count));
    }
  } finally {
    fs.closeSync(handle);
  }
  return hash.digest("hex");
}

export function treeDigest(root, ignoredNames = new Set()) {
  const rows = [];
  const visit = (directory) => {
    const entries = fs.readdirSync(directory, { withFileTypes: true })
      .sort((left, right) => left.name.localeCompare(right.name, "en"));
    for (const entry of entries) {
      if (ignoredNames.has(entry.name)) continue;
      const absolute = path.join(directory, entry.name);
      if (entry.isSymbolicLink()) throw new Error("test tree contains a symbolic link");
      if (entry.isDirectory()) visit(absolute);
      else if (entry.isFile()) {
        const relative = path.relative(root, absolute).split(path.sep).join("/");
        const stat = fs.statSync(absolute);
        rows.push({ path: relative, bytes: stat.size, sha256: sha256File(absolute) });
      } else {
        throw new Error("test tree contains an unsupported filesystem entry");
      }
    }
  };
  visit(root);
  const canonical = rows.map((row) => `${row.path}\0${row.bytes}\0${row.sha256}\n`).join("");
  return {
    sha256: sha256Bytes(canonical),
    fileCount: rows.length,
    bytes: rows.reduce((total, row) => total + row.bytes, 0),
  };
}

export function stableJson(value) {
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${stableJson(value[key])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

export function writeIntegrityEvidence(runRoot, relativePath, payload) {
  const output = requireInside(runRoot, path.join(runRoot, relativePath), "evidence output");
  fs.mkdirSync(path.dirname(output), { recursive: true });
  const payloadSha256 = sha256Bytes(stableJson(payload));
  const envelope = { schemaVersion: 1, payloadSha256, payload };
  const bytes = Buffer.from(`${JSON.stringify(envelope, null, 2)}\n`, "utf8");
  const temporary = `${output}.tmp`;
  const handle = fs.openSync(temporary, "wx");
  try {
    fs.writeFileSync(handle, bytes);
    fs.fsyncSync(handle);
  } finally {
    fs.closeSync(handle);
  }
  fs.renameSync(temporary, output);
  return { output, sha256: sha256File(output), payloadSha256 };
}

export async function runCommand(command, args, options = {}) {
  const timeoutMs = options.timeoutMs ?? 120_000;
  return await new Promise((resolve, reject) => {
    const child = spawn(command, args, {
      cwd: options.cwd,
      env: options.env,
      windowsHide: true,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const stdout = [];
    const stderr = [];
    child.stdout.on("data", (chunk) => stdout.push(chunk));
    child.stderr.on("data", (chunk) => stderr.push(chunk));
    const timer = setTimeout(() => {
      child.kill();
      reject(new Error(`${command} timed out`));
    }, timeoutMs);
    child.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
    child.once("exit", (code, signal) => {
      clearTimeout(timer);
      const result = {
        code,
        signal,
        stdout: Buffer.concat(stdout).toString("utf8"),
        stderr: Buffer.concat(stderr).toString("utf8"),
      };
      if (code === 0) resolve(result);
      else {
        const details = [result.stderr.trim(), result.stdout.trim()].filter(Boolean).join("\n").slice(-4_000);
        reject(new Error(`${command} failed (${code ?? signal}): ${details}`));
      }
    });
  });
}

function githubHeaders() {
  return {
    Accept: "application/vnd.github+json",
    "User-Agent": "codex-switch-v0.3-release-e2e",
    "X-GitHub-Api-Version": "2022-11-28",
  };
}

async function fetchGithubJsonViaCli(parsed) {
  const resource = `${parsed.pathname.replace(/^\//, "")}${parsed.search}`;
  const fallback = await runCommand("gh", ["api", resource], { timeoutMs: 60_000 });
  if (fallback.stdout.length > 2 * 1024 * 1024) throw new Error("GitHub API response is too large");
  return JSON.parse(fallback.stdout);
}

const GITHUB_RETRY_DELAYS_MS = [250, 500, 1_000];
const GITHUB_RETRYABLE_STATUS = new Set([408, 429, 500, 502, 503, 504]);

async function fetchGithubWithRetry(url, options) {
  let lastError;
  for (let attempt = 0; attempt <= GITHUB_RETRY_DELAYS_MS.length; attempt += 1) {
    try {
      const response = await fetch(url, options);
      if (!GITHUB_RETRYABLE_STATUS.has(response.status) || attempt === GITHUB_RETRY_DELAYS_MS.length) {
        return response;
      }
      await response.body?.cancel();
      lastError = new Error(`GitHub request returned transient HTTP ${response.status}`);
    } catch (error) {
      lastError = error;
      if (attempt === GITHUB_RETRY_DELAYS_MS.length) throw error;
    }
    await new Promise((resolve) => setTimeout(resolve, GITHUB_RETRY_DELAYS_MS[attempt]));
  }
  throw lastError ?? new Error("GitHub request retry loop failed without an error");
}

export async function fetchGithubJson(url) {
  const parsed = new URL(url);
  if (parsed.protocol !== "https:" || parsed.host !== "api.github.com") {
    throw new Error("GitHub API URL is outside the fixed public origin");
  }
  let response;
  try {
    response = await fetchGithubWithRetry(parsed, { headers: githubHeaders(), redirect: "error" });
  } catch {
    // Node's fetch occasionally loses the GitHub connection on this Windows
    // release host. The authenticated gh transport uses the same fixed API
    // origin and gives the live gate a second, independently implemented path.
    return await fetchGithubJsonViaCli(parsed);
  }
  if (response.status === 403 || response.status === 429) return await fetchGithubJsonViaCli(parsed);
  if (!response.ok) throw new Error(`GitHub API returned HTTP ${response.status}`);
  const text = await response.text();
  if (text.length > 2 * 1024 * 1024) throw new Error("GitHub API response is too large");
  return JSON.parse(text);
}

export async function resolvePublicRelease(repository, tag, options = {}) {
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository) || !/^v\d+\.\d+\.\d+$/.test(tag)) {
    throw new Error("release repository or tag is invalid");
  }
  const release = await fetchGithubJson(`${GITHUB_API}/repos/${repository}/releases/tags/${tag}`);
  if (release.tag_name !== tag || release.draft || release.prerelease || !release.published_at || !release.html_url) {
    throw new Error(`${tag} is not a public stable GitHub Release`);
  }
  const assets = (release.assets ?? []).filter((asset) => asset.name === "codex-switch.exe");
  if (assets.length !== 1) throw new Error(`${tag} must contain exactly one codex-switch.exe asset`);
  const asset = assets[0];
  const apiDigest = typeof asset.digest === "string" ? asset.digest.match(/^sha256:([0-9a-f]{64})$/i)?.[1]?.toLowerCase() : undefined;
  const pinnedDigest = options.expectedSha256?.toLowerCase();
  if (pinnedDigest && !/^[0-9a-f]{64}$/.test(pinnedDigest)) throw new Error(`${tag} pinned asset digest is invalid`);
  if (apiDigest && pinnedDigest && apiDigest !== pinnedDigest) throw new Error(`${tag} public asset digest changed from its release pin`);
  const digest = apiDigest ?? pinnedDigest;
  const expectedUrl = `https://github.com/${repository}/releases/download/${tag}/codex-switch.exe`;
  if (!digest || asset.browser_download_url !== expectedUrl || !Number.isSafeInteger(asset.size) || asset.size <= 0 || asset.size > 64 * 1024 * 1024) {
    throw new Error(`${tag} asset metadata is invalid`);
  }
  const tagCommit = await resolvePublicTagCommit(repository, tag);
  return {
    tag,
    tagCommit,
    releaseId: release.id,
    releaseUrl: release.html_url,
    publishedAt: release.published_at,
    asset: {
      assetId: asset.id,
      bytes: asset.size,
      sha256: digest,
      digestSource: apiDigest ? (pinnedDigest ? "github-and-release-pin" : "github") : "release-pin",
      downloadUrl: expectedUrl,
    },
  };
}

export async function resolvePublicTagCommit(repository, tag) {
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository) || !/^v\d+\.\d+\.\d+$/.test(tag)) {
    throw new Error("tag repository or name is invalid");
  }
  const ref = await fetchGithubJson(`${GITHUB_API}/repos/${repository}/git/ref/tags/${encodeURIComponent(tag)}`);
  let object = ref.object;
  for (let depth = 0; depth < 3 && object?.type === "tag"; depth += 1) {
    const tagObject = await fetchGithubJson(object.url);
    object = tagObject.object;
  }
  if (object?.type !== "commit" || !/^[0-9a-f]{40}$/i.test(object.sha ?? "")) {
    throw new Error(`${tag} public tag did not resolve to a commit`);
  }
  return object.sha.toLowerCase();
}

export async function downloadBoundAsset(release, destination) {
  if (fs.existsSync(destination)) throw new Error("asset destination already exists");
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  const temporary = `${destination}.partial`;
  if (fs.existsSync(temporary)) throw new Error("asset partial destination already exists");
  try {
    try {
      const response = await fetchGithubWithRetry(release.asset.downloadUrl, {
        headers: { "User-Agent": "codex-switch-v0.3-release-e2e" },
        redirect: "follow",
      });
      if (!response.ok || !response.body) throw new Error(`GitHub asset download returned HTTP ${response.status}`);
      const finalUrl = new URL(response.url);
      if (finalUrl.protocol !== "https:" || !ALLOWED_DOWNLOAD_HOSTS.has(finalUrl.hostname)) {
        throw new Error("GitHub asset redirected outside the fixed public origins");
      }
      const handle = fs.openSync(temporary, "wx");
      try {
        const reader = response.body.getReader();
        let total = 0;
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          total += value.byteLength;
          if (total > release.asset.bytes || total > 64 * 1024 * 1024) throw new Error("GitHub asset exceeded its bound size");
          fs.writeSync(handle, Buffer.from(value));
        }
        fs.fsyncSync(handle);
      } finally {
        fs.closeSync(handle);
      }
    } catch {
      fs.rmSync(temporary, { force: true });
      const fallback = await runCommand("curl.exe", [
        "--fail", "--silent", "--show-error", "--location",
        "--proto", "=https", "--proto-redir", "=https",
        "--max-redirs", "5", "--max-filesize", `${64 * 1024 * 1024}`,
        "--max-time", "300", "--output", temporary,
        "--write-out", "%{url_effective}", release.asset.downloadUrl,
      ], { timeoutMs: 360_000 });
      const finalUrl = new URL(fallback.stdout.trim());
      if (finalUrl.protocol !== "https:" || !ALLOWED_DOWNLOAD_HOSTS.has(finalUrl.hostname)) {
        throw new Error("GitHub asset fallback redirected outside the fixed public origins");
      }
    }

    const metadata = fs.lstatSync(temporary);
    const actual = sha256File(temporary);
    if (!metadata.isFile() || metadata.isSymbolicLink() || metadata.size !== release.asset.bytes || actual !== release.asset.sha256) {
      throw new Error("downloaded GitHub asset does not match public metadata");
    }
    fs.renameSync(temporary, destination);
    return { bytes: metadata.size, sha256: actual };
  } catch (error) {
    fs.rmSync(temporary, { force: true });
    throw error;
  }
}

export function copyBoundExecutable(source, destination, expectedSha256) {
  if (!path.isAbsolute(source) || !/^[0-9a-f]{64}$/.test(expectedSha256)) {
    throw new Error("local executable binding is invalid");
  }
  const sourceMetadata = fs.lstatSync(source);
  if (!sourceMetadata.isFile() || sourceMetadata.isSymbolicLink() || sourceMetadata.size <= 0 || sourceMetadata.size > 64 * 1024 * 1024) {
    throw new Error("local executable source is invalid");
  }
  if (sha256File(source) !== expectedSha256) throw new Error("local executable does not match its immutable SHA-256 pin");
  if (fs.existsSync(destination)) throw new Error("asset destination already exists");
  fs.mkdirSync(path.dirname(destination), { recursive: true });
  const temporary = `${destination}.partial`;
  fs.copyFileSync(source, temporary, fs.constants.COPYFILE_EXCL);
  const copiedMetadata = fs.lstatSync(temporary);
  const actual = sha256File(temporary);
  if (!copiedMetadata.isFile() || copiedMetadata.isSymbolicLink() || copiedMetadata.size !== sourceMetadata.size || actual !== expectedSha256) {
    fs.rmSync(temporary, { force: true });
    throw new Error("copied local executable does not match its source binding");
  }
  const handle = fs.openSync(temporary, "r+");
  try {
    fs.fsyncSync(handle);
  } finally {
    fs.closeSync(handle);
  }
  fs.renameSync(temporary, destination);
  return { bytes: copiedMetadata.size, sha256: actual };
}

export async function readPeVersion(executable) {
  const command = [
    "$ErrorActionPreference='Stop'",
    "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)",
    "$value=(Get-Item -LiteralPath $env:V030_PE_PATH).VersionInfo",
    "[ordered]@{fileVersion=$value.FileVersion;productVersion=$value.ProductVersion}|ConvertTo-Json -Compress",
  ].join(";");
  const result = await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command], {
    env: { ...process.env, V030_PE_PATH: executable },
  });
  return JSON.parse(result.stdout.trim());
}

export function versionTriplet(value) {
  return String(value ?? "").match(/\d+\.\d+\.\d+/)?.[0] ?? null;
}

export function isolatedEnvironment(root, extra = {}, allowNetwork = false) {
  const environment = {};
  for (const key of ["SystemRoot", "WINDIR", "ComSpec", "PATH", "PATHEXT", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"]) {
    if (process.env[key]) environment[key] = process.env[key];
  }
  if (allowNetwork) {
    for (const key of ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "http_proxy", "https_proxy", "all_proxy", "no_proxy"]) {
      if (process.env[key]) environment[key] = process.env[key];
    }
  } else {
    environment.HTTP_PROXY = "http://127.0.0.1:9";
    environment.HTTPS_PROXY = "http://127.0.0.1:9";
    environment.ALL_PROXY = "http://127.0.0.1:9";
    environment.NO_PROXY = "127.0.0.1,localhost,tauri.localhost,::1,[::1]";
  }
  const user = path.join(root, "user");
  const appdata = path.join(root, "appdata");
  const localappdata = path.join(root, "localappdata");
  const temp = path.join(root, "temp");
  for (const directory of [user, appdata, localappdata, temp]) fs.mkdirSync(directory, { recursive: true });
  return {
    ...environment,
    USERPROFILE: user,
    HOME: user,
    APPDATA: appdata,
    LOCALAPPDATA: localappdata,
    TEMP: temp,
    TMP: temp,
    ...extra,
  };
}

export class CdpClient {
  constructor(url) {
    this.url = url;
    this.nextId = 1;
    this.pending = new Map();
    this.eventHandlers = new Map();
  }

  async connect(timeoutMs = 10_000) {
    this.socket = new WebSocket(this.url);
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("CDP websocket connection timed out")), timeoutMs);
      this.socket.addEventListener("open", () => { clearTimeout(timer); resolve(); }, { once: true });
      this.socket.addEventListener("error", () => { clearTimeout(timer); reject(new Error("CDP websocket connection failed")); }, { once: true });
    });
    this.socket.addEventListener("message", (event) => {
      const message = JSON.parse(String(event.data));
      if (!message.id) {
        for (const handler of this.eventHandlers.get(message.method) ?? []) handler(message.params ?? {});
        return;
      }
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(`CDP error: ${JSON.stringify(message.error)}`));
      else pending.resolve(message.result);
    });
  }

  on(method, handler) {
    if (typeof handler !== "function") throw new Error("CDP event handler must be a function");
    const handlers = this.eventHandlers.get(method) ?? [];
    handlers.push(handler);
    this.eventHandlers.set(method, handlers);
    return () => this.eventHandlers.set(method, handlers.filter((candidate) => candidate !== handler));
  }

  call(method, params = {}, timeoutMs = 30_000) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP ${method} timed out`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (value) => { clearTimeout(timer); resolve(value); },
        reject: (error) => { clearTimeout(timer); reject(error); },
      });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  async evaluate(expression, timeoutMs = 30_000) {
    const result = await this.call("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }, timeoutMs);
    if (result.exceptionDetails) throw new Error("CDP expression failed");
    return result.result.value;
  }

  close() {
    try { this.socket?.close(); } catch {}
  }
}

export const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));

// Launching the product with discarded stdio throws away the only first-party
// explanation of a failed start. Every launch is observed instead: child output is
// streamed to the run-owned log directory and lifecycle events are recorded so a
// readiness timeout can report why the process never served CDP.
export function spawnObservedProduct(executable, { cwd, env, logDirectory, label }) {
  fs.mkdirSync(logDirectory, { recursive: true });
  const stdoutPath = path.join(logDirectory, `${label}-stdout.log`);
  const stderrPath = path.join(logDirectory, `${label}-stderr.log`);
  const child = spawn(executable, [], {
    cwd,
    env,
    windowsHide: true,
    stdio: ["ignore", "pipe", "pipe"],
  });
  const state = {
    label,
    pid: child.pid ?? null,
    exited: false,
    exitCode: null,
    exitSignal: null,
    spawnError: null,
    stdoutPath,
    stderrPath,
  };
  child.stdout.pipe(fs.createWriteStream(stdoutPath));
  child.stderr.pipe(fs.createWriteStream(stderrPath));
  child.on("exit", (code, signal) => {
    state.exited = true;
    state.exitCode = code;
    state.exitSignal = signal;
  });
  child.on("error", (error) => {
    state.exited = true;
    state.spawnError = String(error?.message ?? error);
  });
  return { child, state };
}

function tailFile(file, bytes = 4000) {
  try {
    const size = fs.statSync(file).size;
    if (size === 0) return "";
    const handle = fs.openSync(file, "r");
    try {
      const length = Math.min(size, bytes);
      const buffer = Buffer.allocUnsafe(length);
      fs.readSync(handle, buffer, 0, length, size - length);
      return buffer.toString("utf8").trim();
    } finally {
      fs.closeSync(handle);
    }
  } catch {
    return "";
  }
}

// Answers the questions a bare "fetch failed" cannot: is the product still alive,
// did a WebView2 host ever start, and is anything listening on the debug port.
export async function captureLaunchDiagnostics(port, executable, observed) {
  const diagnostics = {
    port,
    process: observed ? { ...observed.state } : null,
    stdoutTail: observed ? tailFile(observed.state.stdoutPath) : "",
    stderrTail: observed ? tailFile(observed.state.stderrPath) : "",
  };
  try {
    diagnostics.productProcesses = await processRowsForExecutable(executable);
  } catch (error) {
    diagnostics.productProcesses = `unavailable: ${String(error?.message ?? error)}`;
  }
  const command = [
    "$ErrorActionPreference='SilentlyContinue'",
    "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)",
    "$port=[int]$env:V030_DEBUG_PORT",
    "$listeners=@(Get-NetTCPConnection -LocalPort $port -State Listen -ErrorAction SilentlyContinue|ForEach-Object{[ordered]@{address=$_.LocalAddress;owner=$_.OwningProcess}})",
    "$webview=@(Get-Process -Name 'msedgewebview2' -ErrorAction SilentlyContinue|ForEach-Object{[ordered]@{pid=$_.Id}})",
    "[ordered]@{listeners=$listeners;webview2=$webview}|ConvertTo-Json -Compress -Depth 4",
  ].join(";");
  try {
    const result = await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command], {
      env: { ...process.env, V030_DEBUG_PORT: String(port) },
      timeoutMs: 20_000,
    });
    diagnostics.host = JSON.parse(result.stdout.trim() || "{}");
  } catch (error) {
    diagnostics.host = `unavailable: ${String(error?.message ?? error)}`;
  }
  return diagnostics;
}

export async function waitForTauriTarget(port, predicate = undefined, timeoutMs = 60_000, options = {}) {
  const { observed, executable } = options;
  const deadline = Date.now() + timeoutMs;
  let lastError = "";
  const endpoints = [
    `http://127.0.0.1:${port}/json/list`,
    `http://localhost:${port}/json/list`,
    `http://[::1]:${port}/json/list`,
  ];
  const fail = async (reason) => {
    const diagnostics = executable ? await captureLaunchDiagnostics(port, executable, observed) : { observed: observed?.state };
    const error = new Error(`${reason}\n${JSON.stringify(diagnostics, null, 2)}`);
    error.diagnostics = diagnostics;
    throw error;
  };
  while (Date.now() < deadline) {
    // A dead child can never serve CDP; waiting out the full timeout only hides the cause.
    if (observed?.state.exited) {
      await fail(
        observed.state.spawnError
          ? `product failed to start: ${observed.state.spawnError}`
          : `product exited before serving CDP (code=${observed.state.exitCode}, signal=${observed.state.exitSignal})`,
      );
    }
    for (const endpoint of endpoints) {
      try {
        const response = await fetch(endpoint, { signal: AbortSignal.timeout(1000) });
        const targets = await response.json();
        for (const target of targets) {
          if (target.type !== "page" || target.url !== "http://tauri.localhost/" || !target.webSocketDebuggerUrl) continue;
          if (!predicate || await predicate(target)) return target;
        }
      } catch (error) {
        lastError = String(error);
      }
    }
    await sleep(250);
  }
  await fail(`WebView2 target did not become ready: ${lastError.slice(-300)}`);
}

export async function waitForDebugPortClosed(port, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  const endpoints = [
    `http://127.0.0.1:${port}/json/list`,
    `http://localhost:${port}/json/list`,
    `http://[::1]:${port}/json/list`,
  ];
  while (Date.now() < deadline) {
    let observed = false;
    for (const endpoint of endpoints) {
      try {
        const response = await fetch(endpoint, { signal: AbortSignal.timeout(500) });
        const targets = await response.json();
        observed ||= targets.some((target) => target.type === "page" && target.url === "http://tauri.localhost/");
      } catch {
        // A closed listener on any loopback family is the desired terminal state.
      }
    }
    if (!observed) {
      return;
    }
    await sleep(200);
  }
  throw new Error("the test-owned WebView2 debugging endpoint remained after graceful close");
}

export function invokeExpression(command, args = undefined) {
  return `(async()=>{const invoke=window.__TAURI_INTERNALS__?.invoke??window.__TAURI__?.core?.invoke??window.__TAURI__?.invoke;if(typeof invoke!=="function")throw new Error("Tauri invoke unavailable");return await invoke(${JSON.stringify(command)}${args === undefined ? "" : `,${JSON.stringify(args)}`});})()`;
}

export async function processRowsForExecutable(executable) {
  const command = [
    "$ErrorActionPreference='Stop'",
    "[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)",
    "$target=[IO.Path]::GetFullPath($env:V030_PROCESS_PATH)",
    "$rows=@(Get-Process -Name 'codex-switch' -ErrorAction SilentlyContinue|ForEach-Object{try{$candidate=$_.Path;$pidValue=$_.Id}catch{return};if($candidate -and [string]::Equals([IO.Path]::GetFullPath($candidate),$target,[StringComparison]::OrdinalIgnoreCase)){[ordered]@{pid=$pidValue}}})",
    "@($rows)|ConvertTo-Json -Compress",
  ].join(";");
  const result = await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command], {
    env: { ...process.env, V030_PROCESS_PATH: executable },
    timeoutMs: 10_000,
  });
  const parsed = JSON.parse(result.stdout.trim() || "[]");
  return Array.isArray(parsed) ? parsed : [parsed];
}

export async function waitForNoExecutableProcess(executable, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if ((await processRowsForExecutable(executable)).length === 0) return;
    await sleep(250);
  }
  throw new Error("the exact test executable did not exit after a graceful close request");
}

export async function closeMainWindowExact(pid, executable) {
  const command = [
    "$ErrorActionPreference='Stop'",
    "$p=Get-Process -Id ([int]$env:V030_PROCESS_PID)",
    "$actual=$p.Path",
    "if(-not [StringComparer]::OrdinalIgnoreCase.Equals([IO.Path]::GetFullPath($actual),[IO.Path]::GetFullPath($env:V030_PROCESS_PATH))){throw 'process identity changed'}",
    "if(-not $p.CloseMainWindow()){throw 'window did not accept WM_CLOSE'}",
  ].join(";");
  await runCommand("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", command], {
    env: { ...process.env, V030_PROCESS_PID: String(pid), V030_PROCESS_PATH: executable },
    timeoutMs: 15_000,
  });
}

export function assertEvidenceHasNoAbsoluteWindowsPath(value) {
  const text = typeof value === "string" ? value : JSON.stringify(value);
  if (/(?:^|[\s"'])\\\\[^\\\s]+\\|(?:^|[\s"'])[A-Za-z]:[\\/]/m.test(text)) {
    throw new Error("evidence contains an absolute Windows path");
  }
}
