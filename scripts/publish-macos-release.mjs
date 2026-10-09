import { createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { validateSbom } from './check-sbom.mjs';

export const VERSION = '0.5.0-macos.1';
export const BUNDLE_VERSION = '0.5.0';
export const SUBTLE_LICENSE_SHA256 = 'cc0332a88c2ea21d5f3c1298f966120f4c95196871c3f6bb4fcf615508b93fa1';
export const TAG = 'v' + VERSION;
const REPOSITORY = 'mingisrookie/codex-switch';
const BUNDLE_ID = 'local.codexswitch.desktop';
const ROOT = fileURLToPath(new URL('..', import.meta.url));
const ARCHITECTURES = { aarch64: 'aarch64-apple-darwin', x64: 'x86_64-apple-darwin' };
const CHECKS = ['file', 'lipo', 'plist', 'codesign', 'hdiutilVerify', 'mountedBundleMatches', 'nativeStartup', 'licenseResources'];
const LIFECYCLE = ['sessionStarted', 'appReady', 'exitRequested', 'sessionEnded'];
const sha = (bytes) => createHash('sha256').update(bytes).digest('hex');
const requireCondition = (condition, message) => { if (!condition) throw new Error(message); };
const readJson = (filename) => {
  const stat = fs.lstatSync(filename);
  requireCondition(stat.isFile() && !stat.isSymbolicLink() && stat.size <= 10 * 1024 * 1024,
    'invalid release JSON file');
  return JSON.parse(fs.readFileSync(filename, 'utf8'));
};
const sameList = (a, b) => JSON.stringify(a) === JSON.stringify(b);

export function validateMacVersionPlist(xml) {
  // Use the standard plist parser already required by native bundle validation.
  // The checked-in override only owns version keys, not executable or bundle identity.
  const result = spawnSync(process.platform === 'win32' ? 'python' : 'python3', [
    '-c', 'import json,plistlib,sys; print(json.dumps(plistlib.loads(sys.stdin.buffer.read())))',
  ], { input: xml, encoding: 'utf8', timeout: 10000, maxBuffer: 256 * 1024 });
  requireCondition(!result.error && result.status === 0, 'macOS version override is not a valid plist');
  const plist = JSON.parse(result.stdout);
  const expected = {
    CFBundleShortVersionString: BUNDLE_VERSION,
    CFBundleVersion: BUNDLE_VERSION,
    CodexSwitchReleaseVersion: VERSION,
  };
  requireCondition(plist && typeof plist === 'object'
    && sameList(Object.keys(plist).sort(), Object.keys(expected).sort())
    && Object.entries(expected).every(([key, value]) => plist[key] === value),
  'macOS plist must map the exact prerelease to the locked numeric Apple bundle versions');
  return { releaseVersion: VERSION, bundleVersion: BUNDLE_VERSION };
}

export function checkVersions(root = ROOT) {
  const pkg = readJson(path.join(root, 'package.json'));
  const lock = readJson(path.join(root, 'package-lock.json'));
  const config = readJson(path.join(root, 'src-tauri/tauri.conf.json'));
  const overlay = readJson(path.join(root, 'src-tauri/tauri.macos.conf.json'));
  const cargo = fs.readFileSync(path.join(root, 'src-tauri/Cargo.toml'), 'utf8');
  const cargoLock = fs.readFileSync(path.join(root, 'src-tauri/Cargo.lock'), 'utf8');
  const packageBlock = cargo.split(/^\[package\]\s*$/m)[1]?.split(/^\[/m)[0];
  const cargoVersion = packageBlock?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  const lockedRoot = cargoLock.split('[[package]]').find((block) =>
    /^name\s*=\s*"codex-switch"\s*$/m.test(block));
  const cargoLockVersion = lockedRoot?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  requireCondition([pkg.version, lock.version, lock.packages?.['']?.version,
    config.version, cargoVersion, cargoLockVersion].every((value) => value === VERSION),
  'package, lockfile, Cargo and Tauri versions must all equal ' + VERSION);
  requireCondition(config.identifier === BUNDLE_ID && config.productName === 'ChatGPT Switch',
    'unexpected release bundle identity');
  requireCondition(overlay.bundle?.active === true
    && sameList([...overlay.bundle.targets].sort(), ['app', 'dmg'])
    && overlay.bundle.macOS?.signingIdentity === '-'
    && overlay.bundle.macOS?.minimumSystemVersion === '12.0'
    && overlay.bundle.macOS?.bundleVersion === BUNDLE_VERSION
    && overlay.bundle.macOS?.infoPlist === 'Info.macos.plist',
  'macOS bundle overlay does not match the reviewed ad-hoc preview');
  requireCondition(sameList(overlay.bundle.resources,
    { 'resources/SUBTLE-LICENSE.txt': 'SUBTLE-LICENSE.txt' }),
  'macOS bundle must embed the reviewed subtle copyright and license text');
  const licensePath = path.join(root, 'src-tauri/resources/SUBTLE-LICENSE.txt');
  const licenseStat = fs.lstatSync(licensePath);
  requireCondition(licenseStat.isFile() && !licenseStat.isSymbolicLink() && licenseStat.size === 1581
    && sha(fs.readFileSync(licensePath)) === SUBTLE_LICENSE_SHA256,
  'subtle copyright and license text differs from its reviewed distribution bytes');
  const plistPath = path.join(root, 'src-tauri/Info.macos.plist');
  const plistStat = fs.lstatSync(plistPath);
  requireCondition(plistStat.isFile() && !plistStat.isSymbolicLink() && plistStat.size <= 8192,
    'invalid macOS version override file');
  validateMacVersionPlist(fs.readFileSync(plistPath, 'utf8'));
  const notes = path.join(root, 'docs/releases/' + TAG + '.md');
  requireCondition(fs.statSync(notes).size > 100, 'reviewed release notes are missing');
  return { version: VERSION, tag: TAG, notes };
}

export function requirePublishContext(env, head, tagHead) {
  requireCondition(env.GITHUB_ACTIONS === 'true' && env.GITHUB_EVENT_NAME === 'push'
    && env.GITHUB_REPOSITORY === REPOSITORY && env.GITHUB_REF === 'refs/tags/' + TAG
    && env.GITHUB_REF_NAME === TAG, 'publication is restricted to the exact macOS tag push');
  requireCondition(/^[a-f0-9]{40}$/.test(env.GITHUB_SHA ?? '')
    && head === env.GITHUB_SHA && tagHead === head,
  'checked-out source, tag and workflow commit must match');
}

export function validateAssets(directory, commit) {
  requireCondition(/^[a-f0-9]{40}$/.test(commit ?? ''), 'invalid release source commit');
  const rootStat = fs.lstatSync(directory);
  requireCondition(rootStat.isDirectory() && !rootStat.isSymbolicLink(), 'invalid release directory');
  const expected = Object.keys(ARCHITECTURES).flatMap((architecture) => {
    const name = 'codex-switch_' + VERSION + '_' + architecture + '.dmg';
    return [name, name + '.sha256', name + '.verification.json', name + '.startup.json',
      'codex-switch_macos_' + architecture + '.cdx.json'];
  }).sort();
  const actual = fs.readdirSync(directory).sort();
  requireCondition(sameList(actual, expected),
    'release must contain exactly the two DMGs, their checksums/evidence and Darwin SBOMs');
  const assets = new Map();
  for (const name of expected) {
    const filename = path.join(directory, name);
    const stat = fs.lstatSync(filename);
    requireCondition(stat.isFile() && !stat.isSymbolicLink() && stat.size > 0
      && stat.size < 512 * 1024 * 1024, 'invalid or oversized release asset');
    assets.set(name, { name, bytes: stat.size, sha256: sha(fs.readFileSync(filename)) });
  }
  for (const [architecture, target] of Object.entries(ARCHITECTURES)) {
    const name = 'codex-switch_' + VERSION + '_' + architecture + '.dmg';
    const dmg = assets.get(name);
    requireCondition(dmg.bytes > 1024, 'DMG is unexpectedly small');
    const checksum = fs.readFileSync(path.join(directory, name + '.sha256'), 'utf8');
    requireCondition(checksum === dmg.sha256 + '  ' + name + '\n', 'DMG checksum mismatch');
    const evidence = readJson(path.join(directory, name + '.verification.json'));
    requireCondition(evidence.schemaVersion === 1 && evidence.releaseVersion === VERSION
      && evidence.tag === TAG && evidence.commit === commit
      && evidence.architecture === architecture && evidence.target === target
      && evidence.bundleIdentifier === BUNDLE_ID && evidence.bundleVersion === BUNDLE_VERSION
      && evidence.bundleShortVersion === BUNDLE_VERSION
      && evidence.minimumSystemVersion === '12.0'
      && evidence.signature === 'adhoc' && evidence.notarized === false,
    'bundle verification identity does not match the release source');
    requireCondition(CHECKS.every((name) => evidence.checks?.[name] === true),
      'a required final-bundle gate did not pass');
    requireCondition(sameList(evidence.bundledLicenses,
      { 'SUBTLE-LICENSE.txt': SUBTLE_LICENSE_SHA256 }),
    'required subtle copyright and license bytes were not verified in the final DMG');
    requireCondition(evidence.dmg?.name === name && evidence.dmg.bytes === dmg.bytes
      && evidence.dmg.sha256 === dmg.sha256, 'verified DMG bytes changed');
    requireCondition(/^[a-f0-9]{64}$/.test(evidence.executableSha256 ?? '')
      && /^[a-f0-9]{64}$/.test(evidence.bundleTreeSha256 ?? ''), 'missing application digests');
    const startup = readJson(path.join(directory, name + '.startup.json'));
    requireCondition(startup.schemaVersion === 1 && startup.releaseVersion === VERSION
      && startup.bundleVersion === BUNDLE_VERSION && startup.bundleShortVersion === BUNDLE_VERSION
      && startup.architecture === architecture && startup.bundleIdentifier === BUNDLE_ID
      && startup.executableSha256 === evidence.executableSha256
      && sameList(startup.lifecycle, LIFECYCLE) && startup.normalQuit === true
      && startup.exitCode === 0 && startup.codexHomeUnchanged === true
      && startup.isolatedHome === true && startup.realClientStarted === false
      && startup.isolatedKeychainVerified === true,
    'native startup evidence is missing or belongs to different application bytes');
    requireCondition(startup.nativeQuit?.method === 'NSRunningApplication.terminate'
      && startup.nativeQuit.reason === 'nativeQuit' && startup.nativeQuit.prevented === false
      && startup.nativeQuit.ownedIdentityVerified === true,
    'native startup must prove the AppKit quit passed the native shutdown reservation');
    const rejected = startup.windowsUpdaterRejected;
    requireCondition(rejected?.argument === '--codex-switch-apply-update'
      && rejected.exitCode === 1 && rejected.deadlineSeconds === 15
      && Number.isInteger(rejected.elapsedMilliseconds)
      && rejected.elapsedMilliseconds >= 0 && rejected.elapsedMilliseconds < 15000
      && sameList(rejected.lifecycle, ['sessionStarted', 'sessionEnded'])
      && rejected.endReason === 'updateStartupHelper' && rejected.appReadyObserved === false
      && rejected.codexHomeUnchanged === true
      && /^[a-f0-9]{64}$/.test(rejected.codexHomeBeforeSha256 ?? '')
      && rejected.codexHomeAfterSha256 === rejected.codexHomeBeforeSha256,
    'native Windows updater rejection evidence is missing or invalid');
    validateSbom(readJson(path.join(directory, 'codex-switch_macos_' + architecture + '.cdx.json')), VERSION);
  }
  return [...assets.values()];
}

function execute(program, args) {
  const result = spawnSync(program, args, { cwd: ROOT, encoding: 'utf8', timeout: 180000,
    maxBuffer: 20 * 1024 * 1024, stdio: ['ignore', 'pipe', 'pipe'] });
  requireCondition(!result.error && result.status === 0, program + ' operation failed');
  return result.stdout.trim();
}

async function publicApi(endpoint, allowMissing = false) {
  const response = await fetch('https://api.github.com/repos/' + REPOSITORY + endpoint, {
    headers: { Accept: 'application/vnd.github+json', 'X-GitHub-Api-Version': '2022-11-28' },
    signal: AbortSignal.timeout(30000),
  });
  if (allowMissing && response.status === 404) return null;
  requireCondition(response.ok, 'public GitHub release read failed (' + response.status + ')');
  return response.json();
}

function latestIdentity(release) {
  requireCondition(release && !release.draft && !release.prerelease && release.tag_name === 'v0.4.0',
    'Windows Latest must remain the reviewed stable v0.4.0 release');
  requireCondition(release.assets?.some((asset) => asset.name === 'codex-switch.exe'),
    'Windows Latest executable is missing');
  return {
    id: release.id, tag: release.tag_name, publishedAt: release.published_at,
    assets: release.assets.map((asset) => ({
      id: asset.id, name: asset.name, bytes: asset.size, digest: asset.digest ?? null,
    })).sort((a, b) => a.name.localeCompare(b.name)),
  };
}

function validateRemote(release, expected, draft) {
  requireCondition(release.tag_name === TAG && release.prerelease === true && release.draft === draft,
    'remote release draft/prerelease identity is incorrect');
  requireCondition(sameList(release.assets.map((asset) => asset.name).sort(),
    expected.map((asset) => asset.name).sort()), 'remote release asset set changed');
  for (const asset of expected) {
    const remote = release.assets.find((entry) => entry.name === asset.name);
    requireCondition(remote.size === asset.bytes && remote.state === 'uploaded',
      'remote release asset upload is incomplete');
    if (remote.digest) requireCondition(remote.digest === 'sha256:' + asset.sha256,
      'remote release asset digest mismatch');
  }
}

export function readDraftRelease(expected, run = execute) {
  const read = (args, stage) => {
    try {
      return JSON.parse(run('gh', args));
    } catch {
      // Child stderr or malformed response bodies can contain private release data.
      throw new Error(stage + ' failed');
    }
  };
  // The tag REST endpoint only returns published releases. Resolve the authenticated
  // draft through gh, then bind its numeric API endpoint before reading asset metadata.
  const view = read(['release', 'view', TAG, '--repo', REPOSITORY,
    '--json', 'apiUrl,isDraft,isPrerelease,tagName'], 'draft release discovery');
  requireCondition(view?.tagName === TAG && view.isDraft === true && view.isPrerelease === true,
    'draft release discovery identity is incorrect');
  const prefix = 'https://api.github.com/repos/' + REPOSITORY + '/releases/';
  const suffix = typeof view.apiUrl === 'string' && view.apiUrl.startsWith(prefix)
    ? view.apiUrl.slice(prefix.length) : '';
  const id = Number(suffix);
  requireCondition(/^[1-9][0-9]*$/.test(suffix) && Number.isSafeInteger(id)
    && view.apiUrl === prefix + id, 'draft release API URL is not the expected numeric repository endpoint');
  const draft = read(['api', 'repos/' + REPOSITORY + '/releases/' + id],
    'draft release metadata read');
  requireCondition(draft?.id === id && draft.url === view.apiUrl,
    'draft release metadata identity does not match discovery');
  validateRemote(draft, expected, true);
  return draft;
}

async function verifyPublicDownloads(release, expected) {
  for (const asset of expected) {
    const remote = release.assets.find((entry) => entry.name === asset.name);
    const expectedUrl = 'https://github.com/' + REPOSITORY + '/releases/download/' + TAG + '/' + asset.name;
    requireCondition(remote.browser_download_url === expectedUrl, 'unexpected public asset URL');
    const response = await fetch(expectedUrl, { signal: AbortSignal.timeout(180000) });
    requireCondition(response.ok && response.body, 'public asset download failed');
    const hasher = createHash('sha256');
    let bytes = 0;
    for await (const block of response.body) {
      bytes += block.byteLength;
      requireCondition(bytes <= asset.bytes, 'public asset exceeds the verified byte count');
      hasher.update(block);
    }
    requireCondition(bytes === asset.bytes && hasher.digest('hex') === asset.sha256,
      'publicly downloaded release bytes do not match the native build');
  }
}

async function publish(directory) {
  const versions = checkVersions();
  const head = execute('git', ['rev-parse', 'HEAD']);
  const tagHead = execute('git', ['rev-parse', '--verify', TAG + '^{commit}']);
  requirePublishContext(process.env, head, tagHead);
  requireCondition(Boolean(process.env.GH_TOKEN), 'release token is unavailable');
  const assets = validateAssets(directory, head);
  const before = latestIdentity(await publicApi('/releases/latest'));
  requireCondition(await publicApi('/releases/tags/' + TAG, true) === null,
    'this release already exists; refusing to replace published assets');
  // Upload as a draft first. Failed upload/readback cannot expose a partial public release.
  execute('gh', ['release', 'create', TAG,
    ...assets.map((asset) => path.resolve(directory, asset.name)),
    '--repo', REPOSITORY, '--verify-tag', '--draft', '--prerelease', '--latest=false',
    '--title', 'ChatGPT Switch ' + TAG, '--notes-file', versions.notes]);
  readDraftRelease(assets);
  const downloaded = fs.mkdtempSync(path.join(os.tmpdir(), 'codex-switch-draft-'));
  try {
    execute('gh', ['release', 'download', TAG, '--repo', REPOSITORY,
      '--dir', downloaded, '--pattern', '*']);
    requireCondition(sameList(validateAssets(downloaded, head), assets),
      'uploaded draft assets failed byte-for-byte readback');
  } finally {
    fs.rmSync(downloaded, { recursive: true, force: true });
  }
  execute('gh', ['release', 'edit', TAG, '--repo', REPOSITORY,
    '--draft=false', '--prerelease', '--latest=false']);
  console.log('MACOS_RELEASE_PUBLISHED https://github.com/' + REPOSITORY + '/releases/tag/' + TAG);
  let published;
  for (let attempt = 0; attempt < 5; attempt += 1) {
    published = await publicApi('/releases/tags/' + TAG, true);
    if (published) break;
    await new Promise((resolve) => setTimeout(resolve, 2000));
  }
  requireCondition(published, 'published release is not yet publicly readable');
  validateRemote(published, assets, false);
  await verifyPublicDownloads(published, assets);
  const after = latestIdentity(await publicApi('/releases/latest'));
  requireCondition(sameList(before, after), 'Windows Latest changed during macOS publication');
  const evidence = {
    schemaVersion: 1, releaseVersion: VERSION, bundleVersion: BUNDLE_VERSION, tag: TAG, commit: head,
    releaseUrl: published.html_url, prerelease: true, latest: false,
    releaseNotesSha256: sha(fs.readFileSync(versions.notes)),
    publicAssetReadback: assets, windowsLatestBefore: before, windowsLatestAfter: after,
  };
  fs.writeFileSync(path.join(ROOT, 'publication-evidence.json'), JSON.stringify(evidence, null, 2) + '\n',
    { encoding: 'utf8', flag: 'wx' });
  console.log('MACOS_PUBLIC_READBACK_OK assets=' + assets.length + ' windowsLatest=' + after.tag);
}

async function main() {
  const [mode, directory = 'release', ...extra] = process.argv.slice(2);
  requireCondition(extra.length === 0, 'unexpected publisher arguments');
  if (mode === '--check-version') {
    checkVersions();
    console.log('MACOS_VERSION_OK ' + VERSION);
  } else if (mode === '--check-assets') {
    checkVersions();
    const commit = process.env.GITHUB_SHA ?? execute('git', ['rev-parse', 'HEAD']);
    const assets = validateAssets(directory, commit);
    console.log('MACOS_ASSETS_OK count=' + assets.length + ' commit=' + commit);
  } else if (mode === '--publish') {
    await publish(directory);
  } else {
    throw new Error('Usage: publish-macos-release.mjs --check-version|--check-assets|--publish [release-dir]');
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main().catch((error) => { console.error('MACOS_RELEASE_FAILED: ' + error.message); process.exitCode = 1; });
}
