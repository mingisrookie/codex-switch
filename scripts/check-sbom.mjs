import { randomUUID } from 'node:crypto';
import { readFile, writeFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

function requireCondition(condition, message) {
  if (!condition) throw new Error(message);
}

const serialNumberPattern = /^urn:uuid:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function requireSerialNumber(bom) {
  requireCondition(typeof bom.serialNumber === 'string' && serialNumberPattern.test(bom.serialNumber),
    'CycloneDX serialNumber must be a UUID URN for GitHub SBOM attestation');
}

export function prepareSbom(input, version, sourceRef) {
  const bom = structuredClone(input);
  const component = bom.metadata?.component;
  const stableRef = `pkg:cargo/codex-switch@${version}`;
  requireCondition(component?.name === 'codex-switch' && component.version === version,
    'SBOM root name/version does not match the current project');
  requireCondition(component['bom-ref'] === sourceRef || component['bom-ref'] === stableRef,
    'SBOM source component is not the current build root');
  requireCondition(component.purl === stableRef || component.purl === `${stableRef}?download_url=file://.`,
    'SBOM source component has an unexpected package URL');
  // cargo-cyclonedx 0.5.9 also emits this crate's build targets as nested
  // metadata components (generator.rs:create_toplevel_component). Normalize only
  // the known crate/targets; never remove arbitrary dependency provenance.
  const replacements = new Map([[component['bom-ref'], stableRef]]);
  component['bom-ref'] = stableRef;
  component.purl = stableRef;
  const targets = new Map([
    ['codex-switch', { type: 'application', source: 'src/main.rs' }],
    ['codex_switch_lib', { type: 'library', source: 'src/lib.rs' }],
  ]);
  const children = component.components ?? [];
  requireCondition(Array.isArray(children) && children.length <= targets.size,
    'SBOM root has unexpected build target components');
  const seenTargets = new Set();
  children.forEach((child, index) => {
    const target = targets.get(child.name);
    requireCondition(target && !seenTargets.has(child.name) && child.version === version
      && child.type === target.type && !child.components,
      'SBOM contains an unexpected nested build target');
    seenTargets.add(child.name);
    const targetRef = `${stableRef}#${target.source}`;
    requireCondition(child['bom-ref'] === `${sourceRef} bin-target-${index}` || child['bom-ref'] === targetRef,
      'SBOM build target does not belong to the current source root');
    requireCondition(child.purl === `${stableRef}?download_url=file://.#${target.source}` || child.purl === targetRef,
      'SBOM build target source URL is not recognized');
    replacements.set(child['bom-ref'], targetRef);
    child['bom-ref'] = targetRef;
    child.purl = targetRef;
  });
  if (Array.isArray(bom.dependencies)) {
    for (const dependency of bom.dependencies) {
      dependency.ref = replacements.get(dependency.ref) ?? dependency.ref;
      if (Array.isArray(dependency.dependsOn)) {
        dependency.dependsOn = dependency.dependsOn.map((ref) => replacements.get(ref) ?? ref);
      }
    }
  }
  // cargo-cyclonedx may omit this optional standard field, but our pinned
  // actions/attest parser requires it. Preserve valid IDs and never overwrite
  // malformed metadata to make a document appear valid.
  if (bom.serialNumber === undefined) bom.serialNumber = `urn:uuid:${randomUUID()}`;
  validateSbom(bom, version);
  return bom;
}

export function validateSbom(bom, version) {
  requireCondition(bom?.bomFormat === 'CycloneDX', 'SBOM bomFormat must be CycloneDX');
  requireCondition(['1.4', '1.5', '1.6'].includes(bom.specVersion), 'Unsupported CycloneDX schema version');
  requireSerialNumber(bom);
  const root = bom.metadata?.component;
  requireCondition(root?.name === 'codex-switch' && root.version === version,
    'SBOM root component must match the current codex-switch version');
  requireCondition(root['bom-ref'] === `pkg:cargo/codex-switch@${version}`,
    'SBOM root identity must be normalized before publication');
  requireCondition(Array.isArray(bom.components) && bom.components.length > 0,
    'SBOM must contain dependency components');
  const refs = new Set();
  for (const component of [root, ...(root.components ?? []), ...bom.components]) {
    requireCondition(typeof component['bom-ref'] === 'string' && component['bom-ref'].length > 0,
      'SBOM component identity is missing');
    requireCondition(!refs.has(component['bom-ref']), 'SBOM contains duplicate component identities');
    refs.add(component['bom-ref']);
  }
  requireCondition(Array.isArray(bom.dependencies) && bom.dependencies.length > 0,
    'SBOM dependency graph is missing');
  const edges = new Set();
  for (const dependency of bom.dependencies) {
    requireCondition(refs.has(dependency.ref) && !edges.has(dependency.ref),
      'SBOM dependency node is missing or duplicated');
    edges.add(dependency.ref);
    // CycloneDX generators omit dependsOn for dependency nodes with no listed edges.
    // Missing is valid; malformed values and every actual unknown reference still fail.
    const dependsOn = dependency.dependsOn === undefined ? [] : dependency.dependsOn;
    requireCondition(Array.isArray(dependsOn)
      && dependsOn.every((ref) => refs.has(ref)), 'SBOM dependency edge refers to an unknown component');
  }
  requireCondition(edges.has(root['bom-ref']), 'SBOM dependency graph omits the root');
  const forbidden = /(?:\bsk-[A-Za-z0-9_-]{12,}|authorization\s*[:=]\s*bearer|file:\/|(?:^|[^A-Za-z0-9])[A-Za-z]:[\\/]|\/(?:home|Users|workspace)\/)/i;
  function inspect(value) {
    if (typeof value === 'string') requireCondition(!forbidden.test(value), 'SBOM contains local-path or credential-shaped content');
    else if (Array.isArray(value)) value.forEach(inspect);
    else if (value && typeof value === 'object') Object.values(value).forEach(inspect);
  }
  inspect(bom);
  return { format: bom.bomFormat, spec: bom.specVersion, components: bom.components.length };
}

async function main() {
  const args = process.argv.slice(2);
  const prepare = args[0] === '--prepare';
  if (prepare) args.shift();
  requireCondition(args.length <= 1 && !args[0]?.startsWith('--'), 'Usage: check-sbom.mjs [--prepare] [file]');
  const input = resolve(args[0] ?? 'src-tauri/codex-switch.cdx.json');
  const pkg = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));
  const raw = await readFile(input, 'utf8');
  requireCondition(Buffer.byteLength(raw, 'utf8') <= 10 * 1024 * 1024, 'SBOM exceeds the size limit');
  let bom = JSON.parse(raw);
  if (prepare) {
    // fileURLToPath is required for Windows drive letters and escaped paths.
    const sourceRef = `path+${pathToFileURL(fileURLToPath(new URL('../src-tauri', import.meta.url))).href}#codex-switch@${pkg.version}`;
    bom = prepareSbom(bom, pkg.version, sourceRef);
    await writeFile(input, `${JSON.stringify(bom, null, 2)}\n`, 'utf8');
  }
  const result = validateSbom(bom, pkg.version);
  console.log(`SBOM_OK format=${result.format} spec=${result.spec} components=${result.components}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => { console.error(error.message); process.exitCode = 1; });
}
