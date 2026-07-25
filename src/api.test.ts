import { beforeEach, describe, expect, it, vi } from 'vitest';

const { invoke, Channel } = vi.hoisted(() => {
  class MockChannel<T> {
    onmessage: (message: T) => void;

    constructor(onmessage: (message: T) => void) {
      this.onmessage = onmessage;
    }
  }

  return { invoke: vi.fn(), Channel: MockChannel };
});

vi.mock('@tauri-apps/api/core', () => ({ Channel, invoke }));

import {
  checkForUpdates,
  deleteManagedSessions,
  getAppStatus,
  getUpdateStartupNotice,
  importPlusRuntime,
  installUpdate,
  installSkill,
  loadBackupDashboard,
  listSkills,
  loadDashboard,
  loadRuntimeDashboard,
  loadSessionDashboard,
  saveSkillConfig,
  switchRuntime,
} from './api';

describe('dashboard API', () => {
  beforeEach(() => invoke.mockReset());

  it('keeps successful domains when one dashboard scan fails', async () => {
    invoke.mockImplementation((command: string) => {
      if (command === 'scan_managed_sessions') {
        return Promise.reject(new Error('managed scan failed'));
      }
      const values: Record<string, unknown> = {
        scan_codex_home: { root: 'C:\\Users\\alice\\.codex' },
        scan_sessions: { threadCount: 4, sessionJsonlCount: 3 },
        list_runtimes: [],
        scan_runtime_status: {
          activeRuntimeId: null,
          confidence: 'unknown',
          authMode: null,
          modelProvider: null,
          detectedAtMs: 1,
        },
        list_backups: [],
        list_operation_records: [],
      };
      return Promise.resolve(values[command]);
    });

    const dashboard = await loadDashboard();

    expect(dashboard.codexHome).toMatchObject({ status: 'ready' });
    expect(dashboard.sessions).toMatchObject({ status: 'ready' });
    expect(dashboard.managedSessions).toMatchObject({
      status: 'error',
      error: 'managed scan failed',
    });
    expect(dashboard.runtimes).toMatchObject({ status: 'ready', data: [] });
    expect(dashboard.runtimeStatus).toMatchObject({ status: 'ready' });
    expect(dashboard.backups).toMatchObject({ status: 'ready', data: [] });
    expect(dashboard.operations).toMatchObject({ status: 'ready', data: [] });
    expect(invoke).toHaveBeenCalledTimes(7);
    expect(invoke).toHaveBeenCalledWith('list_operation_records', { limit: 20 });
  });

  it('refreshes runtime-facing domains without scanning sessions', async () => {
    invoke.mockImplementation((command: string) => Promise.resolve({
      list_runtimes: [],
      scan_runtime_status: {
        activeRuntimeId: 'relay',
        confidence: 'exact',
        authMode: 'apikey',
        modelProvider: 'openai_custom',
        detectedAtMs: 1,
      },
      list_operation_records: [],
    }[command]));

    const dashboard = await loadRuntimeDashboard();

    expect(dashboard.runtimeStatus).toMatchObject({ status: 'ready' });
    expect(invoke).toHaveBeenCalledTimes(4);
    expect(invoke).not.toHaveBeenCalledWith('scan_sessions');
    expect(invoke).not.toHaveBeenCalledWith('scan_managed_sessions');
    expect(invoke).not.toHaveBeenCalledWith('list_backups');
  });

  it('loads session domains independently when the session page becomes visible', async () => {
    invoke.mockImplementation((command: string) => Promise.resolve({
      scan_sessions: { threadCount: 4, sessionJsonlCount: 3 },
      scan_managed_sessions: { totalCount: 4, archivedCount: 0, sessions: [] },
    }[command]));

    const dashboard = await loadSessionDashboard();

    expect(dashboard.sessions).toMatchObject({ status: 'ready' });
    expect(invoke).toHaveBeenCalledTimes(2);
    expect(invoke).not.toHaveBeenCalledWith('list_backups');
    expect(invoke).not.toHaveBeenCalledWith('list_runtimes');
  });

  it('loads expensive backup verification only through its explicit loader', async () => {
    invoke.mockResolvedValue([]);

    const dashboard = await loadBackupDashboard();

    expect(dashboard.backups).toMatchObject({ status: 'ready', data: [] });
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith('list_backups');
  });

  it('passes overwrite confirmation explicitly when importing the account runtime', async () => {
    invoke.mockResolvedValue({ id: 'plus' });

    await importPlusRuntime(true);

    expect(invoke).toHaveBeenCalledWith('import_plus_runtime', { confirmOverwrite: true });
  });

  it('uses fixed commands for app version, update checks, and installation', async () => {
    invoke.mockResolvedValue(undefined);

    await getAppStatus();
    await checkForUpdates();
    await installUpdate();
    await getUpdateStartupNotice();

    expect(invoke).toHaveBeenNthCalledWith(1, 'get_app_status');
    expect(invoke).toHaveBeenNthCalledWith(2, 'check_for_updates');
    expect(invoke).toHaveBeenNthCalledWith(3, 'install_update');
    expect(invoke).toHaveBeenNthCalledWith(4, 'get_update_startup_notice');
  });

  it('passes the hard-delete confirmation under the backend confirmed field', async () => {
    invoke.mockResolvedValue({ selectedCount: 1 });

    await deleteManagedSessions(['thread-a'], true);

    expect(invoke).toHaveBeenCalledWith('delete_managed_sessions', {
      ids: ['thread-a'],
      confirmed: true,
    });
  });

  it('uses fixed typed commands for skill listing, installation, and configuration', async () => {
    invoke.mockResolvedValue([]);

    await listSkills();
    await installSkill('image2', true);
    await saveSkillConfig({
      skillId: 'grokSearch',
      baseUrl: 'https://research.example.com',
      apiKey: 'sk-fake',
    });

    expect(invoke).toHaveBeenNthCalledWith(1, 'list_skills');
    expect(invoke).toHaveBeenNthCalledWith(2, 'install_skill', {
      skillId: 'image2',
      confirmReplace: true,
    });
    expect(invoke).toHaveBeenNthCalledWith(3, 'save_skill_config', {
      input: {
        skillId: 'grokSearch',
        baseUrl: 'https://research.example.com',
        apiKey: 'sk-fake',
      },
    });
  });

  it('passes a per-switch channel and forwards backend progress events', async () => {
    invoke.mockResolvedValue({ changed: true });
    const events: string[] = [];

    await switchRuntime('relay', (event) => events.push(event.phase));

    const payload = invoke.mock.calls[0][1] as {
      runtimeId: string;
      onProgress: { onmessage: (event: { phase: string }) => void };
    };
    expect(invoke).toHaveBeenCalledWith('switch_runtime', {
      runtimeId: 'relay',
      onProgress: expect.any(Channel),
    });
    payload.onProgress.onmessage({ phase: 'detectingApp' });
    expect(events).toEqual(['detectingApp']);
  });
});
