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
if (executable[0] !== 0x4d || executable[1] !== 0x5a) {
  fail('release artifact is not a Windows PE executable');
}

if (process.platform === 'win32') {
  const productVersion = execFileSync(
    'powershell.exe',
    [
      '-NoLogo',
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      '(Get-Item -LiteralPath $env:CODEX_SWITCH_RELEASE_EXE).VersionInfo.ProductVersion',
    ],
    {
      encoding: 'utf8',
      env: {
        ...process.env,
        CODEX_SWITCH_RELEASE_EXE: executablePath,
      },
    },
  ).trim();
  if (productVersion !== expectedVersion) {
    fail(`PE ProductVersion ${productVersion} does not match ${expectedVersion}`);
  }
}

const ascii = executable.toString('latin1').toLowerCase();
const utf16 = executable.toString('utf16le').toLowerCase();
const forbidden = [
  ['workspace path', root],
  ['workspace path', root.replaceAll('\\', '/')],
  ['user home path', homedir()],
  ['user home path', homedir().replaceAll('\\', '/')],
  ['GitHub token prefix', 'ghp_'],
  ['GitHub token prefix', 'gho_'],
  ['GitHub token prefix', 'ghu_'],
  ['GitHub token prefix', 'ghs_'],
  ['GitHub token prefix', 'ghr_'],
  ['GitHub token prefix', 'github_pat_'],
].map(([label, value]) => [label, value.toLowerCase()]);
for (const [label, marker] of forbidden) {
  if (ascii.includes(marker) || utf16.includes(marker)) {
    fail(`release artifact contains forbidden ${label}`);
  }
}

console.log(
  `Release contract passed: v${expectedVersion}, ${executableSize} bytes, ${executablePath}`,
);
