import { beforeEach, describe, expect, it, vi } from 'vitest';

const invoke = vi.hoisted(() => vi.fn());

vi.mock('@tauri-apps/api/core', () => ({ invoke }));

import {
  deleteManagedSessions,
  importPlusRuntime,
  installSkill,
  listSkills,
  loadDashboard,
  saveSkillConfig,
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

  it('passes overwrite confirmation explicitly when importing the account runtime', async () => {
    invoke.mockResolvedValue({ id: 'plus' });

    await importPlusRuntime(true);

    expect(invoke).toHaveBeenCalledWith('import_plus_runtime', { confirmOverwrite: true });
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
});
