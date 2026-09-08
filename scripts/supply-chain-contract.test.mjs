import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { checkDocumentation, resolveDocumentationTarget } from './check-doc-links.mjs';
import { prepareSbom, validateSbom } from './check-sbom.mjs';

const version = '0.4.0';
const sourceRef = 'path+file:///fixture/project/src-tauri#codex-switch@0.4.0';
const stableRef = 'pkg:cargo/codex-switch@0.4.0';
function fixture() {
  return {
    bomFormat: 'CycloneDX', specVersion: '1.5', version: 1,
    metadata: { component: { name: 'codex-switch', version, 'bom-ref': sourceRef,
      purl: `${stableRef}?download_url=file://.` } },
    components: [{ name: 'serde', version: '1.0.228', 'bom-ref': 'serde', purl: 'pkg:cargo/serde@1.0.228',
      externalReferences: [{ type: 'website', url: 'https://serde.rs' }] }],
    dependencies: [{ ref: sourceRef, dependsOn: ['serde'] }, { ref: 'serde', dependsOn: [] }],
  };
}

test('SBOM preparation removes only the known workspace identity and preserves graph edges', () => {
  const original = fixture();
  const prepared = prepareSbom(original, version, sourceRef);
  assert.equal(prepared.metadata.component['bom-ref'], stableRef);
  assert.equal(prepared.metadata.component.purl, stableRef);
  assert.equal(prepared.dependencies[0].ref, stableRef);
  assert.deepEqual(prepared.dependencies[0].dependsOn, ['serde']);
  assert.deepEqual(prepared.components, original.components);
  assert.equal(original.metadata.component['bom-ref'], sourceRef);
  assert.deepEqual(prepareSbom(prepared, version, sourceRef), prepared);
  assert.equal(validateSbom(prepared, version).components, 1);
});

test('SBOM rejects mismatched project version, foreign source identity and missing graph', () => {
  assert.throws(() => prepareSbom(fixture(), '0.3.5', sourceRef), /name\/version/);
  assert.throws(() => prepareSbom(fixture(), version, 'another-root'), /build root/);
  const bom = prepareSbom(fixture(), version, sourceRef);
  bom.dependencies = [];
  assert.throws(() => validateSbom(bom, version), /graph/);
});

test('SBOM refuses unknown dependency edges and duplicate component identities', () => {
  const bom = prepareSbom(fixture(), version, sourceRef);
  bom.dependencies[0].dependsOn.push('missing');
  assert.throws(() => validateSbom(bom, version), /unknown component/);
  bom.dependencies[0].dependsOn.pop();
  bom.components.push(structuredClone(bom.components[0]));
  assert.throws(() => validateSbom(bom, version), /duplicate component/);
});

for (const localPath of ['file:///fixture/dependency', 'C:\\build\\private', 'G:/project/source', '/home/runner/work/repo']) {
  test(`SBOM rejects a local dependency path rather than silently stripping it (${localPath})`, () => {
    const bom = fixture();
    bom.components[0].externalReferences[0].url = localPath;
    assert.throws(() => prepareSbom(bom, version, sourceRef), /local-path/);
  });
}

test('documentation containment works on Windows and POSIX, including prefix siblings', () => {
  for (const [paths, root] of [[path.win32, 'G:\\repo'], [path.posix, '/repo']]) {
    assert.equal(resolveDocumentationTarget(root, 'README.md', 'docs/guide.md', paths).relative,
      paths.join('docs', 'guide.md'));
    assert.throws(() => resolveDocumentationTarget(root, 'README.md', '../repo-other/file.md', paths), /escapes/);
    assert.throws(() => resolveDocumentationTarget(root, 'README.md', '%zz.md', paths), URIError);
    assert.equal(resolveDocumentationTarget(root, 'README.md', 'https://example.com', paths), null);
    assert.equal(resolveDocumentationTarget(root, 'README.md', '#section', paths), null);
  }
});

test('documentation checker inspects HTML images and ignores fenced examples', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cs-docs-contract-'));
  try {
    fs.writeFileSync(path.join(root, 'README.md'),
      '[ok](guide.md)\n<img src="missing.png" alt="preview" />\n```md\n[example](not-a-file.md)\n```\n');
    fs.writeFileSync(path.join(root, 'guide.md'), '# Guide\n');
    const result = checkDocumentation(root, ['README.md', 'guide.md']);
    assert.equal(result.failures.length, 1);
    assert.match(result.failures[0], /missing.png/);
    fs.writeFileSync(path.join(root, 'missing.png'), 'fixture');
    assert.equal(checkDocumentation(root, ['README.md', 'guide.md', 'missing.png']).failures.length, 0);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('CI uses the generator basename, validates the normalized SBOM, and preserves the saved-slot token', () => {
  const workflow = fs.readFileSync('.github/workflows/ci.yml', 'utf8');
  assert.match(workflow, /--override-filename codex-switch\.cdx\s*\n/);
  assert.match(workflow, /--spec-version 1\.5/);
  assert.doesNotMatch(workflow, /--override-filename codex-switch\.cdx\.json/);
  assert.match(workflow, /check-sbom\.mjs --prepare src-tauri\/codex-switch\.cdx\.json/);
  assert.match(workflow, /GH_TOKEN: \$\{\{ github\.token \}\}/);
});

test('compatibility inspection stays off the GUI thread and native UI follows the new storage page', () => {
  const source = fs.readFileSync('src-tauri/src/commands.rs', 'utf8');
  const command = source.match(/pub async fn get_runtime_compatibility\(\)[\s\S]*?\n#\[tauri::command\]/)?.[0];
  assert.ok(command);
  assert.match(command, /spawn_blocking/);
  const harness = fs.readFileSync('scripts/v030-product-ui-e2e.mjs', 'utf8');
  assert.match(harness, /高级存储/);
  assert.match(harness, /initializeNativeCodexState/);
});


test('offline cleanup rechecks schema before pinning databases, not inside its writer callback', () => {
  const source = fs.readFileSync('src-tauri/src/commands.rs', 'utf8');
  const callback = source.match(/execute_offline_gc\(&prepared\.plan, \|\| \{([\s\S]*?)\}/)?.[1];
  assert.ok(callback);
  assert.match(callback, /ensure_no_writer/);
  assert.doesNotMatch(callback, /require_advanced_storage|ensure_compatible_storage/);
  assert.match(source, /performing the final offline cleanup write check/);
});


test('visibility restore rechecks compatibility in its final post-backup gate', () => {
  const source = fs.readFileSync('src-tauri/src/session_manager.rs', 'utf8');
  const entry = source.match(/pub\(crate\) fn restore_sessions_visible_detailed_with_prepare[\s\S]*?\n#\[cfg\(test\)\]/)?.[0];
  assert.ok(entry);
  assert.match(entry, /require_advanced_storage_after_writer_check/);
  assert.match(entry, /ensure_codex_still_closed/);
  assert.match(source, /final_gate\("visibility restore"\)[\s\S]*?restore_visible_in_db/);
});


test('CycloneDX nodes may omit dependsOn, but malformed edge values remain invalid', () => {
  const bom = prepareSbom(fixture(), version, sourceRef);
  delete bom.dependencies[1].dependsOn;
  assert.equal(validateSbom(bom, version).components, 1);
  for (const invalid of [null, 'serde', {}]) {
    bom.dependencies[1].dependsOn = invalid;
    assert.throws(() => validateSbom(bom, version), /dependency edge/);
  }
});


test('crate build targets are normalized without losing identities or source subpaths', () => {
  const bom = fixture();
  bom.metadata.component.components = [
    { type: 'library', name: 'codex_switch_lib', version, 'bom-ref': `${sourceRef} bin-target-0`,
      purl: `${stableRef}?download_url=file://.#src/lib.rs` },
    { type: 'application', name: 'codex-switch', version, 'bom-ref': `${sourceRef} bin-target-1`,
      purl: `${stableRef}?download_url=file://.#src/main.rs` },
  ];
  bom.dependencies[0].dependsOn.push(`${sourceRef} bin-target-1`);
  const normalized = prepareSbom(bom, version, sourceRef);
  assert.equal(normalized.metadata.component.components[0].purl, `${stableRef}#src/lib.rs`);
  assert.equal(normalized.dependencies[0].dependsOn.at(-1), `${stableRef}#src/main.rs`);
  assert.deepEqual(prepareSbom(normalized, version, sourceRef), normalized);
  assert.equal(bom.metadata.component.components[0]['bom-ref'], `${sourceRef} bin-target-0`);
  bom.metadata.component.components[0].purl = `${stableRef}?download_url=file:///unrelated/private#src/lib.rs`;
  assert.throws(() => prepareSbom(bom, version, sourceRef), /source URL/);
});


test('SBOM preparation supplies the serialNumber required by the pinned GitHub attestation parser', () => {
  const input = fixture();
  const prepared = prepareSbom(input, version, sourceRef);
  assert.match(prepared.serialNumber, /^urn:uuid:[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  assert.equal(input.serialNumber, undefined);
  assert.deepEqual(prepareSbom(prepared, version, sourceRef), prepared);
  assert.ok(prepared.bomFormat && prepared.serialNumber && prepared.specVersion);
});

test('SBOM preparation preserves a valid existing serialNumber', () => {
  const input = fixture();
  input.serialNumber = 'urn:uuid:12345678-1234-4123-8123-123456789abc';
  assert.equal(prepareSbom(input, version, sourceRef).serialNumber, input.serialNumber);
});

test('SBOM validation rejects omitted or malformed serial numbers before signing', () => {
  for (const serial of [undefined, null, '', 'not-a-uuid', 42, 'urn:uuid:../escape']) {
    const input = prepareSbom(fixture(), version, sourceRef);
    input.serialNumber = serial;
    assert.throws(() => validateSbom(input, version), /serialNumber/);
    if (serial !== undefined) assert.throws(() => prepareSbom(input, version, sourceRef), /serialNumber/);
  }
});
