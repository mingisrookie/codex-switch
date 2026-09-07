import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";
import { spawn } from "node:child_process";

function withDeadline(promise, timeoutMs, message) {
  let timer;
  return Promise.race([
    promise,
    new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(message)), timeoutMs); }),
  ]).finally(() => clearTimeout(timer));
}

// This helper owns only the child it starts for a disposable schema fixture.
// Desktop product shutdown remains on request_app_exit / exact window close.
export async function initializeNativeCodexState(
  codexExe, codexHome, workspace, environment, clientVersion,
  { timeoutMs = 90_000, shutdownTimeoutMs = 10_000, spawnProcess = spawn } = {},
) {
  const child = spawnProcess(codexExe, ["app-server", "--stdio", "--disable", "plugins"], {
    cwd: workspace,
    env: { ...environment, CODEX_HOME: codexHome, CODEX_SQLITE_HOME: codexHome },
    windowsHide: true,
    stdio: ["pipe", "pipe", "pipe"],
  });
  let finished = false;
  let acknowledged = false;
  let resolveAck;
  let rejectAck;
  const acknowledgement = new Promise((resolve, reject) => { resolveAck = resolve; rejectAck = reject; });
  const exited = new Promise((resolve) => {
    child.once("exit", (code, signal) => {
      finished = true;
      if (!acknowledged) rejectAck(new Error(`native Codex exited before initialize (code ${code ?? signal})`));
      resolve({ code, signal });
    });
    child.once("error", () => {
      finished = true;
      rejectAck(new Error("native Codex schema process could not start"));
      resolve({ code: null, signal: "spawn-error" });
    });
  });
  const lines = readline.createInterface({ input: child.stdout });
  let outputBytes = 0;
  // Drain stderr without retaining its potentially sensitive free-form messages.
  for (const stream of [child.stdout, child.stderr]) {
    stream.on("data", (chunk) => {
      outputBytes += chunk.length;
      if (outputBytes > 256 * 1024) rejectAck(new Error("native Codex schema output exceeded its limit"));
    });
  }
  lines.on("line", (line) => {
    let message;
    try { message = JSON.parse(line); } catch { return; }
    if (!message || typeof message !== "object" || message.id !== 1 || acknowledged) return;
    if (!message.result || message.error) {
      rejectAck(new Error("native Codex rejected schema initialization"));
      return;
    }
    acknowledged = true;
    lines.close();
    resolveAck();
  });
  child.stdin.on("error", () => rejectAck(new Error("native Codex schema input closed before acknowledgement")));
  try {
    child.stdin.write(`${JSON.stringify({ id: 1, method: "initialize", params: {
      clientInfo: { name: "codex-switch-product-ui", version: clientVersion },
      capabilities: { experimentalApi: false },
    } })}\n`);
    await withDeadline(acknowledgement, timeoutMs, "native Codex schema initialization timed out");
    child.stdin.end(`${JSON.stringify({ method: "initialized", params: {} })}\n`);
    const status = await withDeadline(exited, shutdownTimeoutMs, "native Codex schema process did not exit after EOF");
    if (status.code !== 0 || !fs.statSync(path.join(codexHome, "state_5.sqlite"), { throwIfNoEntry: false })?.isFile()) {
      throw new Error("native Codex did not create the product UI state database");
    }
  } finally {
    child.stdin.end();
    try {
      if (!finished) {
        // A hung fixture is stopped by its owned ChildProcess handle, never a name/PID search.
        child.kill();
        await withDeadline(exited, shutdownTimeoutMs, "native Codex schema child cleanup timed out");
      }
    } finally {
      lines.close();
      child.stdin.destroy();
      child.stdout.destroy();
      child.stderr.destroy();
    }
  }
}
