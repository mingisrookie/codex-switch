import { readFileSync, statSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { execFileSync } from 'node:child_process';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const readText = (path) => readFileSync(resolve(root, path), 'utf8');
const fail = (message) => {
  throw new Error(`Release contract failed: ${message}`);
};

const packageJson = JSON.parse(readText('package.json'));
const packageLock = JSON.parse(readText('package-lock.json'));
const tauriConfig = JSON.parse(readText('src-tauri/tauri.conf.json'));
const cargoToml = readText('src-tauri/Cargo.toml');
const cargoLock = readText('src-tauri/Cargo.lock');
const cargoVersion = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
const cargoLockVersion = cargoLock.match(
  /\[\[package\]\]\s*\r?\nname = "codex-switch"\s*\r?\nversion = "([^"]+)"/,
)?.[1];

const versions = new Map([
  ['package.json', packageJson.version],
  ['package-lock.json top-level', packageLock.version],
  ['package-lock.json root package', packageLock.packages?.['']?.version],
  ['Cargo.toml', cargoVersion],
  ['Cargo.lock', cargoLockVersion],
  ['tauri.conf.json', tauriConfig.version],
]);
const expectedVersion = packageJson.version;
for (const [source, version] of versions) {
  if (version !== expectedVersion) {
    fail(`${source} has version ${String(version)}, expected ${expectedVersion}`);
  }
}
if (
  process.env.GITHUB_REF_TYPE === 'tag' &&
  process.env.GITHUB_REF_NAME !== `v${expectedVersion}`
) {
  fail(`tag ${process.env.GITHUB_REF_NAME} does not match v${expectedVersion}`);
}

const executablePath = resolve(
  root,
  process.argv[2] ?? 'src-tauri/target/release/codex-switch.exe',
);
const executable = readFileSync(executablePath);
const executableSize = statSync(executablePath).size;
if (executableSize === 0 || executableSize > 64 * 1024 * 1024) {
  fail(`unexpected executable size ${executableSize}`);
}
if (
  executable.length < 0x40 ||
  executable[0] !== 0x4d ||
  executable[1] !== 0x5a
) {
  fail('release artifact is not a Windows PE executable');
}
const peOffset = executable.readUInt32LE(0x3c);
if (
  peOffset > executable.length - 26 ||
  executable.subarray(peOffset, peOffset + 4).compare(Buffer.from('PE\0\0')) !==
    0
) {
  fail('release artifact has an invalid PE header');
}
const machine = executable.readUInt16LE(peOffset + 4);
if (machine !== 0x8664) {
  fail(`release artifact machine 0x${machine.toString(16)} is not x64`);
}
const optionalHeaderMagic = executable.readUInt16LE(peOffset + 24);
if (optionalHeaderMagic !== 0x20b) {
  fail(
    `release artifact optional header 0x${optionalHeaderMagic.toString(16)} is not PE32+`,
  );
}

if (process.platform === 'win32') {
  const versionJson = execFileSync(
    'powershell.exe',
    [
      '-NoLogo',
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      '$version = (Get-Item -LiteralPath $env:CODEX_SWITCH_RELEASE_EXE).VersionInfo; [pscustomobject]@{ ProductVersion = $version.ProductVersion; FileVersion = $version.FileVersion } | ConvertTo-Json -Compress',
    ],
    {
      encoding: 'utf8',
      env: {
        ...process.env,
        CODEX_SWITCH_RELEASE_EXE: executablePath,
      },
    },
  ).trim();
  const versionInfo = JSON.parse(versionJson);
  for (const field of ['ProductVersion', 'FileVersion']) {
    if (versionInfo[field] !== expectedVersion) {
      fail(
        `PE ${field} ${String(versionInfo[field])} does not match ${expectedVersion}`,
      );
    }
  }
}

const ascii = executable.toString('latin1').toLowerCase();
const utf16 = executable.toString('utf16le').toLowerCase();
const forbidden = [
  ['workspace path', root],
  ['workspace path', root.replaceAll('\\', '/')],
  ['user home path', homedir()],
  ['user home path', homedir().replaceAll('\\', '/')],
].map(([label, value]) => [label, value.toLowerCase()]);
for (const [label, marker] of forbidden) {
  if (ascii.includes(marker) || utf16.includes(marker)) {
    fail(`release artifact contains forbidden ${label}`);
  }
}

// The diagnostics sanitizer intentionally embeds bare credential prefixes so it
// can recognize them in user-facing errors. Reject complete token-shaped values
// rather than making that defensive scanner fail the release gate itself.
const forbiddenCredentials = [
  ['GitHub token', /gh[pousr]_[a-z0-9]{20,}/],
  // Fine-grained PATs have a long random body. A shorter threshold produces a
  // false positive when the sanitizer's adjacent prefix table is folded into
  // the release binary (for example `github_pat_ghp_gho_...`).
  ['GitHub token', /github_pat_[a-z0-9_]{60,}/],
];
for (const [label, pattern] of forbiddenCredentials) {
  if (pattern.test(ascii) || pattern.test(utf16)) {
    fail(`release artifact contains forbidden ${label}`);
  }
}

console.log(
  `Release contract passed: v${expectedVersion}, PE32+ x64, ${executableSize} bytes, ${executablePath}`,
);
