import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { checkVersions, readDraftRelease, requirePublishContext, validateAssets, validateMacVersionPlist,
  BUNDLE_VERSION, VERSION, TAG, SUBTLE_LICENSE_SHA256 } from './publish-macos-release.mjs';

const commit = 'a'.repeat(40);
const digest = (value) => createHash('sha256').update(value).digest('hex');
const lifecycle = ['sessionStarted', 'appReady', 'exitRequested', 'sessionEnded'];
const env = {
  GITHUB_ACTIONS: 'true', GITHUB_EVENT_NAME: 'push',
  GITHUB_REPOSITORY: 'mingisrookie/codex-switch', GITHUB_REF: 'refs/tags/' + TAG,
  GITHUB_REF_NAME: TAG, GITHUB_SHA: commit,
};

function fixture(run) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'macos-release-contract-'));
  const writeJson = (name, value) => fs.writeFileSync(path.join(root, name), JSON.stringify(value));
  try {
    for (const [architecture, target] of [
      ['aarch64', 'aarch64-apple-darwin'], ['x64', 'x86_64-apple-darwin'],
    ]) {
      const name = 'codex-switch_' + VERSION + '_' + architecture + '.dmg';
      const bytes = Buffer.alloc(2048, architecture === 'aarch64' ? 1 : 2);
      const sha256 = digest(bytes);
      fs.writeFileSync(path.join(root, name), bytes);
      fs.writeFileSync(path.join(root, name + '.sha256'), sha256 + '  ' + name + '\n');
      const identity = { schemaVersion: 1, releaseVersion: VERSION, architecture,
        bundleVersion: BUNDLE_VERSION, bundleShortVersion: BUNDLE_VERSION,
        bundleIdentifier: 'local.codexswitch.desktop', executableSha256: 'b'.repeat(64) };
      writeJson(name + '.verification.json', {
        ...identity, tag: TAG, commit, target,
        minimumSystemVersion: '12.0', signature: 'adhoc', notarized: false,
        bundledLicenses: { 'SUBTLE-LICENSE.txt': SUBTLE_LICENSE_SHA256 },
        bundleTreeSha256: 'c'.repeat(64), dmg: { name, bytes: bytes.length, sha256 },
        checks: Object.fromEntries(['file', 'lipo', 'plist', 'codesign', 'hdiutilVerify',
          'mountedBundleMatches', 'nativeStartup', 'licenseResources'].map((check) => [check, true])),
      });
      writeJson(name + '.startup.json', {
        ...identity, lifecycle, normalQuit: true, exitCode: 0, codexHomeUnchanged: true,
        nativeQuit: { method: 'NSRunningApplication.terminate', reason: 'nativeQuit',
          prevented: false, ownedIdentityVerified: true },
        isolatedHome: true, realClientStarted: false,
        isolatedKeychainVerified: true,
        windowsUpdaterRejected: {
          argument: '--codex-switch-apply-update', exitCode: 1,
          deadlineSeconds: 15, elapsedMilliseconds: 100,
          lifecycle: ['sessionStarted', 'sessionEnded'], endReason: 'updateStartupHelper',
          appReadyObserved: false, codexHomeUnchanged: true,
          codexHomeBeforeSha256: 'e'.repeat(64), codexHomeAfterSha256: 'e'.repeat(64),
        },
      });
      const ref = 'pkg:cargo/codex-switch@' + VERSION;
      writeJson('codex-switch_macos_' + architecture + '.cdx.json', {
        bomFormat: 'CycloneDX', specVersion: '1.5', version: 1,
        serialNumber: 'urn:uuid:12345678-1234-4123-8123-123456789abc',
        metadata: { component: { name: 'codex-switch', version: VERSION, 'bom-ref': ref } },
        components: [{ name: 'serde', version: '1.0.228', 'bom-ref': 'serde' }],
        dependencies: [{ ref, dependsOn: ['serde'] }, { ref: 'serde', dependsOn: [] }],
      });
    }
    run(root);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

test('source versions and ad-hoc bundle settings are locked consistently', () => {
  assert.equal(checkVersions().version, VERSION);
});

test('Apple plist versions are numeric while the exact prerelease mapping is preserved', () => {
  const xml = fs.readFileSync('src-tauri/Info.macos.plist', 'utf8');
  assert.deepEqual(validateMacVersionPlist(xml), {
    releaseVersion: VERSION, bundleVersion: BUNDLE_VERSION,
  });
  const shortKey = /<key>CFBundleShortVersionString<\/key>\s*<string>0\.5\.0<\/string>/;
  const buildKey = /<key>CFBundleVersion<\/key>\s*<string>0\.5\.0<\/string>/;
  for (const changed of [
    xml.replace(shortKey, '<key>CFBundleShortVersionString</key><string>' + VERSION + '</string>'),
    xml.replace(buildKey, '<key>CFBundleVersion</key><string>' + VERSION + '</string>'),
    xml.replace('<string>' + VERSION + '</string>', '<string>' + BUNDLE_VERSION + '</string>'),
    xml.replace('</dict>', '<key>CFBundleIdentifier</key><string>foreign.bundle</string></dict>'),
  ]) {
    assert.notEqual(changed, xml);
    assert.throws(() => validateMacVersionPlist(changed), /locked numeric Apple bundle versions/);
  }
});

test('source gate rejects an unreviewed plist path and changed effective Apple version', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'macos-version-source-'));
  try {
    for (const filename of ['package.json', 'package-lock.json', 'src-tauri/Cargo.toml',
      'src-tauri/Cargo.lock', 'src-tauri/tauri.conf.json', 'src-tauri/tauri.macos.conf.json',
      'src-tauri/Info.macos.plist', 'src-tauri/resources/SUBTLE-LICENSE.txt',
      'docs/releases/' + TAG + '.md']) {
      const target = path.join(root, filename);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.copyFileSync(filename, target);
    }
    const configFile = path.join(root, 'src-tauri/tauri.macos.conf.json');
    const config = JSON.parse(fs.readFileSync(configFile, 'utf8'));
    config.bundle.macOS.infoPlist = '../foreign.plist';
    fs.writeFileSync(configFile, JSON.stringify(config));
    assert.throws(() => checkVersions(root), /bundle overlay/);
    config.bundle.macOS.infoPlist = 'Info.macos.plist';
    fs.writeFileSync(configFile, JSON.stringify(config));
    const plistFile = path.join(root, 'src-tauri/Info.macos.plist');
    const changed = fs.readFileSync(plistFile, 'utf8').replace(
      '<string>' + BUNDLE_VERSION + '</string>', '<string>0.5.1</string>');
    fs.writeFileSync(plistFile, changed);
    assert.throws(() => checkVersions(root), /locked numeric Apple bundle versions/);
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
});

test('publisher only permits the exact matching tag push in the upstream repository', () => {
  requirePublishContext(env, commit, commit);
  for (const change of [
    { GITHUB_REF: 'refs/heads/feat/macos-release' },
    { GITHUB_REF_NAME: 'v0.5.0' },
    { GITHUB_REPOSITORY: 'fork/codex-switch' },
    { GITHUB_EVENT_NAME: 'workflow_dispatch' },
    { GITHUB_ACTIONS: 'false' },
    { GITHUB_SHA: 'b'.repeat(40) },
  ]) {
    assert.throws(() => requirePublishContext({ ...env, ...change }, commit, commit));
  }
  assert.throws(() => requirePublishContext(env, commit, 'b'.repeat(40)), /commit/);
});

function draftFixture() {
  const id = 407513082;
  const endpoint = 'repos/mingisrookie/codex-switch/releases/' + id;
  const view = { apiUrl: 'https://api.github.com/' + endpoint,
    isDraft: true, isPrerelease: true, tagName: TAG };
  const expected = [{ name: 'fixture.dmg', bytes: 2048, sha256: 'a'.repeat(64) }];
  const draft = { id, url: view.apiUrl, tag_name: TAG, draft: true, prerelease: true,
    assets: expected.map((asset) => ({ name: asset.name, size: asset.bytes,
      digest: 'sha256:' + asset.sha256, state: 'uploaded' })) };
  const calls = [];
  const run = (program, args) => {
    assert.equal(program, 'gh');
    calls.push(args);
    if (args[0] === 'release') {
      assert.deepEqual(args, ['release', 'view', TAG, '--repo', 'mingisrookie/codex-switch',
        '--json', 'apiUrl,isDraft,isPrerelease,tagName']);
      return JSON.stringify(view);
    }
    if (args[1] === 'repos/mingisrookie/codex-switch/releases/tags/' + TAG) {
      throw new Error('HTTP 404: draft is not published');
    }
    assert.deepEqual(args, ['api', endpoint]);
    return JSON.stringify(draft);
  };
  return { view, draft, expected, calls, run };
}

test('authenticated draft lookup uses its numeric ID when the tag REST endpoint returns 404', () => {
  const { view, draft, expected, calls, run } = draftFixture();
  assert.throws(() => run('gh', ['api',
    'repos/mingisrookie/codex-switch/releases/tags/' + TAG]), /HTTP 404/);
  calls.length = 0;
  assert.deepEqual(readDraftRelease(expected, run), draft);
  assert.equal(calls.length, 2);
  assert.equal('https://api.github.com/' + calls[1][1], view.apiUrl);
});

test('draft discovery rejects foreign or noncanonical API paths before any metadata GET', () => {
  const prefix = 'https://api.github.com/repos/mingisrookie/codex-switch/releases/';
  for (const apiUrl of [
    undefined, null, 407513082,
    'https://api.github.com/repos/foreign/codex-switch/releases/407513082',
    'https://api.github.com/repos/mingisrookie/other/releases/407513082',
    'https://example.com/repos/mingisrookie/codex-switch/releases/407513082',
    prefix + 'tags/' + TAG, prefix + '../407513082', prefix + '0', prefix + '-1',
    prefix + '0407513082', prefix + '407513082?x=1', prefix + '407513082/extra',
    prefix + '407513082#fragment', prefix + '407513082%2fassets',
    prefix + '9007199254740992',
  ]) {
    const state = draftFixture();
    state.view.apiUrl = apiUrl;
    assert.throws(() => readDraftRelease(state.expected, state.run), /numeric repository endpoint/);
    assert.equal(state.calls.length, 1);
  }
});

test('draft discovery requires the exact tag and both draft and prerelease flags', () => {
  for (const changed of [
    { tagName: 'v0.4.0' }, { isDraft: false }, { isDraft: 'true' },
    { isPrerelease: false }, { isPrerelease: undefined },
  ]) {
    const state = draftFixture();
    Object.assign(state.view, changed);
    assert.throws(() => readDraftRelease(state.expected, state.run), /discovery identity/);
    assert.equal(state.calls.length, 1);
  }
});

test('numeric draft metadata must retain the discovered ID, URL, tag and private preview state', () => {
  for (const changed of [
    { id: 407513083 }, { id: '407513082' },
    { url: 'https://api.github.com/repos/foreign/codex-switch/releases/407513082' },
    { tag_name: 'v0.4.0' }, { draft: false }, { prerelease: false },
  ]) {
    const state = draftFixture();
    Object.assign(state.draft, changed);
    assert.throws(() => readDraftRelease(state.expected, state.run), /identity/);
    assert.equal(state.calls.length, 2);
  }
});

test('draft metadata still enforces the exact uploaded asset names, bytes, state and digests', () => {
  for (const changed of [
    [], [{ name: 'foreign.dmg' }], [{ size: 2049 }],
    [{ state: 'new' }], [{ digest: 'sha256:' + 'b'.repeat(64) }],
  ]) {
    const state = draftFixture();
    state.draft.assets = changed.map((asset) => ({ ...state.draft.assets[0], ...asset }));
    assert.throws(() => readDraftRelease(state.expected, state.run), /remote release asset/);
  }
});

test('draft read failures identify the stage without exposing child errors or response bodies', () => {
  for (const failedCall of [1, 2]) {
    for (const malformed of [false, true]) {
      const state = draftFixture();
      let calls = 0;
      const run = (program, args) => {
        calls += 1;
        if (calls !== failedCall) return state.run(program, args);
        if (malformed) return 'PRIVATE_RESPONSE_NOT_JSON';
        throw new Error('PRIVATE_CHILD_STDERR');
      };
      const message = failedCall === 1 ? 'draft release discovery failed'
        : 'draft release metadata read failed';
      assert.throws(() => readDraftRelease(state.expected, run), { message });
      assert.equal(calls, failedCall);
    }
  }
});

test('both architecture bundles require complete matching native evidence and SBOMs', () => {
  fixture((root) => assert.equal(validateAssets(root, commit).length, 10));
});

test('mutated DMG bytes cannot pass publication validation', () => {
  fixture((root) => {
    const name = 'codex-switch_' + VERSION + '_aarch64.dmg';
    fs.appendFileSync(path.join(root, name), 'changed');
    assert.throws(() => validateAssets(root, commit), /checksum/);
  });
});

test('publication requires the reviewed license bytes inside each final DMG', () => {
  fixture((root) => {
    const filename = path.join(root, 'codex-switch_' + VERSION + '_x64.dmg.verification.json');
    const evidence = JSON.parse(fs.readFileSync(filename, 'utf8'));
    delete evidence.bundledLicenses;
    fs.writeFileSync(filename, JSON.stringify(evidence));
    assert.throws(() => validateAssets(root, commit), /license bytes/);
    evidence.bundledLicenses = { 'SUBTLE-LICENSE.txt': 'f'.repeat(64) };
    fs.writeFileSync(filename, JSON.stringify(evidence));
    assert.throws(() => validateAssets(root, commit), /license bytes/);
    evidence.bundledLicenses = { 'SUBTLE-LICENSE.txt': SUBTLE_LICENSE_SHA256 };
    evidence.checks.licenseResources = false;
    fs.writeFileSync(filename, JSON.stringify(evidence));
    assert.throws(() => validateAssets(root, commit), /final-bundle gate/);
  });
});

test('Windows assets and missing native architecture artifacts are rejected', () => {
  fixture((root) => {
    fs.writeFileSync(path.join(root, 'codex-switch.exe'), 'fixture');
    assert.throws(() => validateAssets(root, commit), /exactly/);
    fs.unlinkSync(path.join(root, 'codex-switch.exe'));
    fs.unlinkSync(path.join(root, 'codex-switch_' + VERSION + '_x64.dmg'));
    assert.throws(() => validateAssets(root, commit), /exactly/);
  });
});

test('startup evidence must prove normal app exit for the exact executable digest', () => {
  fixture((root) => {
    const filename = path.join(root, 'codex-switch_' + VERSION + '_x64.dmg.startup.json');
    const evidence = JSON.parse(fs.readFileSync(filename, 'utf8'));
    evidence.executableSha256 = 'd'.repeat(64);
    fs.writeFileSync(filename, JSON.stringify(evidence));
    assert.throws(() => validateAssets(root, commit), /startup evidence/);
    evidence.executableSha256 = 'b'.repeat(64);
    evidence.lifecycle = ['sessionStarted', 'appReady'];
    fs.writeFileSync(filename, JSON.stringify(evidence));
    assert.throws(() => validateAssets(root, commit), /startup evidence/);
  });
});

test('publication requires the verified AppKit native shutdown branch', () => {
  fixture((root) => {
    const filename = path.join(root, 'codex-switch_' + VERSION + '_x64.dmg.startup.json');
    const evidence = JSON.parse(fs.readFileSync(filename, 'utf8'));
    const valid = evidence.nativeQuit;
    for (const changed of [undefined, { ...valid, method: 'signal' },
      { ...valid, reason: 'programmaticExit' }, { ...valid, prevented: true },
      { ...valid, ownedIdentityVerified: false }]) {
      evidence.nativeQuit = changed;
      fs.writeFileSync(filename, JSON.stringify(evidence));
      assert.throws(() => validateAssets(root, commit), /native shutdown reservation/);
    }
  });
});

test('publication requires prompt pre-GUI updater rejection with preserved Codex fixture bytes', () => {
  fixture((root) => {
    const filename = path.join(root, 'codex-switch_' + VERSION + '_x64.dmg.startup.json');
    const evidence = JSON.parse(fs.readFileSync(filename, 'utf8'));
    const valid = evidence.windowsUpdaterRejected;
    for (const changed of [undefined, { ...valid, exitCode: 0 },
      { ...valid, elapsedMilliseconds: 15000 }, { ...valid, appReadyObserved: true },
      { ...valid, lifecycle: ['sessionStarted', 'appReady', 'sessionEnded'] },
      { ...valid, endReason: 'runEventExit' }, { ...valid, codexHomeUnchanged: false },
      { ...valid, codexHomeAfterSha256: 'd'.repeat(64) }]) {
      evidence.windowsUpdaterRejected = changed;
      fs.writeFileSync(filename, JSON.stringify(evidence));
      assert.throws(() => validateAssets(root, commit), /updater rejection evidence/);
    }
  });
});

test('a different source commit cannot reuse earlier passing bundle evidence', () => {
  fixture((root) => assert.throws(() => validateAssets(root, 'd'.repeat(40)), /source/));
});

test('publication rejects semver in native bundle fields and numeric-only release identity', () => {
  fixture((root) => {
    const filename = path.join(root, 'codex-switch_' + VERSION + '_x64.dmg.verification.json');
    const evidence = JSON.parse(fs.readFileSync(filename, 'utf8'));
    evidence.bundleVersion = VERSION;
    fs.writeFileSync(filename, JSON.stringify(evidence));
    assert.throws(() => validateAssets(root, commit), /bundle verification identity/);
    evidence.bundleVersion = BUNDLE_VERSION;
    evidence.releaseVersion = BUNDLE_VERSION;
    fs.writeFileSync(filename, JSON.stringify(evidence));
    assert.throws(() => validateAssets(root, commit), /bundle verification identity/);
  });
});

test('Windows release workflow explicitly excludes macOS previews and prerelease Latest', () => {
  const workflow = fs.readFileSync('.github/workflows/ci.yml', 'utf8');
  assert.match(workflow, /startsWith\(github\.ref, 'refs\/tags\/v'\) && !contains\(github\.ref, '-macos\.'\)/);
  assert.match(workflow, /Windows Latest publisher only accepts stable release versions/);
  const mac = fs.readFileSync('.github/workflows/macos-release.yml', 'utf8');
  assert.match(mac, /needs: \[native, windows-regression, supply-chain\]/);
  assert.match(mac, /github\.ref == 'refs\/tags\/v0\.5\.0-macos\.1'/);
  assert.match(mac, /macos-15-intel/);
});
