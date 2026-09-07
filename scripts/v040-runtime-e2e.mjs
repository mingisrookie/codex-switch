import fs from 'node:fs';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { invokeExpression, sha256File, sleep } from './v030-e2e-lib.mjs';

// Runs only inside the caller's disposable, already-verified fresh Codex Home.
// Never invokes switch_runtime: desktop process control is not isolated by CODEX_HOME.
export async function verifyRuntimeCapabilities(cdp, codexHome) {
  const status = await cdp.evaluate(invokeExpression('get_app_status'));
  if (path.resolve(status.codexHome).toLowerCase() !== path.resolve(codexHome).toLowerCase()) {
    throw new Error('Capability fixture does not own the product Home');
  }
  const dbPath = path.join(codexHome, 'state_5.sqlite');
  for (const file of ['auth.json', 'config.toml', 'state_5.sqlite']) {
    if (fs.existsSync(path.join(codexHome, file))) throw new Error('Capability fixture requires a fresh Home');
  }
  const initial = await cdp.evaluate(invokeExpression('get_runtime_compatibility'));
  if (initial.routeConfig !== 'supported' || initial.stateDatabase !== 'absent') throw new Error('Fresh Home was not supported');
  const saved = await cdp.evaluate(invokeExpression('upsert_relay_runtime', {
    input: { baseUrl: 'https://relay.example.invalid/custom-api', model: 'fixture-model', apiKey: 'FIXTURE_ONLY_CAPABILITY_KEY' },
  }));
  if (saved.relayTransport !== 'http' || saved.baseUrl !== 'https://relay.example.invalid/custom-api') {
    throw new Error('Default transport or explicit API path was changed');
  }
  const button = (label) => `Array.from(document.querySelectorAll('button')).find(el => el.textContent?.trim() === ${JSON.stringify(label)})`;
  async function waitUntil(expression, label) {
    const end = Date.now() + 25_000;
    while (Date.now() < end) {
      if (await cdp.evaluate(expression)) return;
      await sleep(100);
    }
    throw new Error(`${label} did not settle`);
  }
  async function click(expression) {
    const point = await cdp.evaluate(`(()=>{const el=${expression};if(!el||el.disabled)throw new Error('control unavailable');el.scrollIntoView({block:'center',inline:'nearest'});const r=el.getBoundingClientRect();const x=r.left+r.width/2,y=r.top+r.height/2;const top=document.elementFromPoint(x,y);if(top!==el&&!el.contains(top))throw new Error('control obscured');return {x,y};})()`);
    await cdp.call('Input.dispatchMouseEvent', { type: 'mousePressed', button: 'left', clickCount: 1, ...point });
    await cdp.call('Input.dispatchMouseEvent', { type: 'mouseReleased', button: 'left', clickCount: 1, ...point });
    await sleep(120);
  }
  const refresh = `document.querySelector('.topbar-actions button[title="刷新"]') || ${button('刷新')}`;
  await click(refresh);
  await waitUntil(`document.body.innerText.includes('fixture-model') && !${button('配置中转站')}?.disabled`, 'saved relay');
  await click(button('配置中转站'));
  await waitUntil(`Boolean(document.querySelector('input[name="relay-transport"][value="http"]')?.checked)`, 'HTTP radio default');
  const keyEmpty = await cdp.evaluate(`document.querySelector('[aria-label="API Key"]').value === ''`);
  if (!keyEmpty) throw new Error('Saved credential was refilled into the editor');
  await click(`document.querySelector('input[name="relay-transport"][value="websocket"]')`);
  await click(button('保存中转站'));
  await waitUntil(`!document.querySelector('.relay-config-panel') && !${button('配置中转站')}?.disabled`, 'transport save');
  const slots = await cdp.evaluate(invokeExpression('list_runtimes'));
  if (slots.find((slot) => slot.id === 'relay')?.relayTransport !== 'websocket') throw new Error('Explicit WSS choice did not persist');

  function schema(sql) {
    const db = new DatabaseSync(dbPath);
    try { db.exec(sql); } finally { db.close(); }
  }
  schema('CREATE TABLE threads (id TEXT, branch TEXT, model_provider TEXT, rollout_path TEXT, archived INTEGER DEFAULT 0, PRIMARY KEY(id, branch));');
  const blocked = await cdp.evaluate(invokeExpression('get_runtime_compatibility'));
  if (blocked.sessionView !== 'blocked' || blocked.advancedStorage !== 'blocked') throw new Error('Composite primary key was not blocked');
  const before = sha256File(dbPath);
  const rejection = await cdp.evaluate(`(async()=>{try {await window.__TAURI_INTERNALS__.invoke('set_session_storage_automatic_cleanup',{enabled:true});return {rejected:false};} catch(error) {return {rejected:true,typed:String(error).includes('runtimeCompatibilityBlocked')};}})()`);
  if (!rejection.rejected || !rejection.typed || before !== sha256File(dbPath)) throw new Error('Unsupported schema write did not fail closed');
  await click(refresh);
  await waitUntil(`Boolean(${button('切换到中转站')}?.disabled) && document.body.innerText.includes('未知结构')`, 'unsupported schema UI gate');
  // Restore only the synthetic fixture so the caller can run the normal UI quiet-window gate.
  schema('DROP TABLE threads; CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT, rollout_path TEXT, archived INTEGER DEFAULT 0);');
  const supported = await cdp.evaluate(invokeExpression('get_runtime_compatibility'));
  if (supported.sessionView !== 'supported' || supported.advancedStorage !== 'supported') throw new Error('Supported schema did not enable capabilities');
  await click(refresh);
  await waitUntil(`!${button('配置中转站')}?.disabled && document.body.innerText.includes('已支持')`, 'supported schema refresh');
  for (const file of ['auth.json', 'config.toml']) {
    if (fs.existsSync(path.join(codexHome, file))) throw new Error('Capability validation changed live login or route config');
  }
  return {
    defaultHttp: true, explicitWebsocketPersisted: true, customApiPathPreserved: true,
    credentialNotRefilled: true, compositeKeyBlocked: true, unsupportedWriteRejected: true,
    rejectedDatabaseBytesPreserved: true, uiSwitchDisabledWhenUnsupported: true,
    validSchemaRecovered: true, authAndRouteFilesRemainAbsent: true,
    nativeRouteSwitchInvoked: false, fixtureOnly: true,
  };
}
