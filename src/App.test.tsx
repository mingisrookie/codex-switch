import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DashboardData } from './types';

const apiMocks = vi.hoisted(() => ({
  importPlusRuntime: vi.fn(),
  upsertRelayRuntime: vi.fn(),
  verifyRelayRuntime: vi.fn(),
  listCodexProcesses: vi.fn(),
  closeCodexProcesses: vi.fn(),
  switchRuntime: vi.fn(),
  dryRunAllSessions: vi.fn(),
  syncAllSessions: vi.fn(),
  deleteManagedSessions: vi.fn(),
  restoreSessionsVisible: vi.fn(),
  restoreBackup: vi.fn(),
  listSkills: vi.fn(),
  installSkill: vi.fn(),
  saveSkillConfig: vi.fn(),
}));

vi.mock('./api', async () => {
  const actual = await vi.importActual<typeof import('./api')>('./api');
  return { ...actual, ...apiMocks };
});

import App from './App';

function dashboardData(): DashboardData {
  return {
    codexHome: {
      status: 'ready',
      data: {
        root: 'C:\\Users\\alice\\.codex',
        sqliteHome: 'C:\\Users\\alice\\.codex',
        authJson: { path: 'auth.json', exists: true, bytes: 4525 },
        configToml: { path: 'config.toml', exists: true, bytes: 6585 },
        stateDb: { path: 'state_5.sqlite', exists: true, bytes: 12496896 },
        logsDb: { path: 'logs_2.sqlite', exists: true, bytes: 681955328 },
        codexDevDb: { path: 'sqlite/codex-dev.db', exists: true, bytes: 98304 },
        sessionsDir: { path: 'sessions', exists: true, bytes: null },
        sessionJsonlCount: 200,
        authSummary: { authMode: 'apikey', topLevelKeys: ['auth_mode'], hasTokensObject: false },
      },
    },
    sessions: {
      status: 'ready',
      data: {
        home: 'C:\\Users\\alice\\.codex',
        threadCount: 429,
        sessionJsonlCount: 200,
        threads: [],
        sessionFiles: [],
      },
    },
    managedSessions: {
      status: 'ready',
      data: {
        currentHome: 'C:\\Users\\alice\\.codex',
        sharedHome: 'C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\shared-sessions',
        totalCount: 1,
        archivedCount: 0,
        sessions: [],
      },
    },
    runtimes: {
      status: 'ready',
      data: [
        {
          id: 'plus', name: 'Codex 账号', kind: 'plus', baseUrl: null, model: 'gpt-5.5',
          createdAtMs: 1, lastUsedAtMs: null, lastVerifiedAtMs: null,
        },
        {
          id: 'relay', name: 'API 中转站', kind: 'relay', baseUrl: 'https://relay.example.com/v1', model: 'gpt-5.5',
          createdAtMs: 2, lastUsedAtMs: 3, lastVerifiedAtMs: 4,
        },
      ],
    },
    runtimeStatus: {
      status: 'ready',
      data: {
        activeRuntimeId: 'relay', confidence: 'exact', authMode: 'apikey',
        modelProvider: 'openai_custom', detectedAtMs: 5,
      },
    },
    backups: {
      status: 'ready',
      data: [{
        backupDir: 'C:\\backups\\safe-1', sourceRoot: 'C:\\Users\\alice\\.codex', reason: 'switch-runtime', createdAtMs: 10,
        fileCount: 4, totalBytes: 4096, verified: true, completeSessions: true,
      }],
    },
    operations: {
      status: 'ready',
      data: [{
        operationId: 'history-1', action: 'switchRuntime', status: 'succeeded', phase: 'complete',
        startedAtMs: 9, completedAtMs: 10, backupDirs: ['C:\\backups\\safe-1'],
        counts: { insertedThreads: 2 },
      }],
    },
  };
}

function deferred<T>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((nextResolve, nextReject) => {
    resolve = nextResolve;
    reject = nextReject;
  });
  return { promise, resolve, reject };
}

describe('App release-hardening UI', () => {
  beforeEach(() => {
    for (const mock of Object.values(apiMocks)) {
      mock.mockReset();
      mock.mockResolvedValue(undefined);
    }
    apiMocks.listCodexProcesses.mockResolvedValue([]);
    apiMocks.listSkills.mockResolvedValue([]);
    apiMocks.importPlusRuntime.mockResolvedValue({
      id: 'plus', name: 'Codex 账号', kind: 'plus', baseUrl: null, model: 'gpt-5.5',
      createdAtMs: 1, lastUsedAtMs: null, lastVerifiedAtMs: null,
    });
    apiMocks.upsertRelayRuntime.mockResolvedValue({
      id: 'relay', name: 'API 中转站', kind: 'relay', baseUrl: 'https://new.example.com/v1', model: 'gpt-5.5-mini',
      createdAtMs: 2, lastUsedAtMs: null, lastVerifiedAtMs: null,
    });
    apiMocks.dryRunAllSessions.mockResolvedValue({
      toShared: { sourceThreads: 429, targetThreads: 400, newThreads: 2, duplicateThreads: 427 },
      toCurrent: { sourceThreads: 400, targetThreads: 429, newThreads: 1, duplicateThreads: 399 },
    });
    vi.restoreAllMocks();
  });

  it('renders saved, current, and verified as separate runtime states', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const account = await screen.findByRole('article', { name: 'Codex 账号态' });
    const relay = screen.getByRole('article', { name: 'API 中转站态' });
    expect(within(account).getByText('已保存')).toBeTruthy();
    expect(within(account).getByText('非当前')).toBeTruthy();
    expect(within(account).getByText('未验证')).toBeTruthy();
    expect(within(relay).getByText('当前运行')).toBeTruthy();
    expect(within(relay).getByText('已验证')).toBeTruthy();
    expect((within(relay).getByRole('button', { name: '当前为中转站' }) as HTMLButtonElement).disabled).toBe(true);
  });

  it('loads the independent skills page only after the user opens its tab', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    await screen.findByRole('article', { name: 'Codex 账号态' });
    expect(apiMocks.listSkills).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: '技能' }));

    expect(await screen.findByRole('heading', { name: '技能安装与配置' })).toBeTruthy();
    await waitFor(() => expect(apiMocks.listSkills).toHaveBeenCalledTimes(1));
    expect(screen.queryByRole('button', { name: '刷新' })).toBeNull();
  });

  it('does not disable a switch when the backend only has a mode-level match', async () => {
    const data = dashboardData();
    if (data.runtimeStatus.status !== 'ready') throw new Error('fixture must include runtime status');
    data.runtimeStatus.data.confidence = 'mode';
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const relay = await screen.findByRole('article', { name: 'API 中转站态' });
    expect(within(relay).getByText('模式匹配')).toBeTruthy();
    expect((within(relay).getByRole('button', { name: '重新应用中转站' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('uses a controlled password form instead of prompt for relay credentials', async () => {
    const prompt = vi.spyOn(window, 'prompt');
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));

    const dialog = screen.getByRole('dialog', { name: '配置 API 中转站' });
    const key = within(dialog).getByLabelText('API Key') as HTMLInputElement;
    expect(key.type).toBe('password');
    fireEvent.change(within(dialog).getByLabelText('Base URL'), { target: { value: 'https://new.example.com/v1' } });
    fireEvent.change(within(dialog).getByLabelText('模型'), { target: { value: 'gpt-5.5-mini' } });
    fireEvent.change(key, { target: { value: 'sk-secret' } });
    fireEvent.click(within(dialog).getByRole('button', { name: '保存中转站' }));

    await waitFor(() => expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith({
      baseUrl: 'https://new.example.com/v1', model: 'gpt-5.5-mini', apiKey: 'sk-secret',
    }));
    expect(prompt).not.toHaveBeenCalled();
    expect(screen.queryByText('sk-secret')).toBeNull();
  });

  it('submits an empty key explicitly to preserve the existing encrypted relay key', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));

    fireEvent.click(within(screen.getByRole('dialog', { name: '配置 API 中转站' }))
      .getByRole('button', { name: '保存中转站' }));

    await waitFor(() => expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith({
      baseUrl: 'https://relay.example.com/v1', model: 'gpt-5.5', apiKey: '',
    }));
  });

  it('rejects relay URLs with embedded credentials before invoking the backend', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const dialog = screen.getByRole('dialog', { name: '配置 API 中转站' });
    fireEvent.change(within(dialog).getByLabelText('Base URL'), {
      target: { value: 'https://user:secret@relay.example.com/v1' },
    });
    fireEvent.click(within(dialog).getByRole('button', { name: '保存中转站' }));

    expect(await within(dialog).findByText('Base URL 不能包含用户名、密码、查询参数或片段')).toBeTruthy();
    expect(apiMocks.upsertRelayRuntime).not.toHaveBeenCalled();
  });

  it('normalizes a relay host without a scheme to https before saving', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const dialog = screen.getByRole('dialog', { name: '配置 API 中转站' });
    fireEvent.change(within(dialog).getByLabelText('Base URL'), { target: { value: 'relay.example.com/v1' } });
    fireEvent.click(within(dialog).getByRole('button', { name: '保存中转站' }));

    await waitFor(() => expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith(expect.objectContaining({
      baseUrl: 'https://relay.example.com/v1',
    })));
  });

  it('requires an explicit warning confirmation for non-local plain HTTP relays', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(false);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const dialog = screen.getByRole('dialog', { name: '配置 API 中转站' });
    fireEvent.change(within(dialog).getByLabelText('Base URL'), { target: { value: 'http://relay.example.com/v1' } });
    expect(within(dialog).getByRole('status').textContent).toContain('明文传输');
    fireEvent.click(within(dialog).getByRole('button', { name: '保存中转站' }));

    expect(confirm).toHaveBeenCalledWith(expect.stringContaining('明文 HTTP'));
    expect(apiMocks.upsertRelayRuntime).not.toHaveBeenCalled();
  });

  it('keeps the successful receipt and closes the dialog when only refresh fails', async () => {
    const pendingRefresh = deferred<DashboardData>();
    const load = vi.fn()
      .mockResolvedValueOnce(dashboardData())
      .mockReturnValueOnce(pendingRefresh.promise);
    render(<App loadDashboard={load} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    fireEvent.click(within(screen.getByRole('dialog', { name: '配置 API 中转站' }))
      .getByRole('button', { name: '保存中转站' }));

    expect(await screen.findByText('API 中转站已保存')).toBeTruthy();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    const dialogClosedBeforeRefresh = screen.queryByRole('dialog', { name: '配置 API 中转站' }) === null;
    const configureEnabledBeforeRefresh = !(screen.getByRole('button', { name: '配置中转站' }) as HTMLButtonElement).disabled;
    pendingRefresh.reject(new Error('refresh failed'));
    expect(await screen.findByText(/操作已成功，但状态刷新失败：refresh failed/)).toBeTruthy();
    expect(dialogClosedBeforeRefresh).toBe(true);
    expect(configureEnabledBeforeRefresh).toBe(true);
  });

  it('keeps the relay key and shows backend save failures inside the dialog', async () => {
    const pendingRefresh = deferred<DashboardData>();
    apiMocks.upsertRelayRuntime.mockRejectedValue(new Error('relay store unavailable'));
    const load = vi.fn()
      .mockResolvedValueOnce(dashboardData())
      .mockReturnValueOnce(pendingRefresh.promise);
    render(<App loadDashboard={load} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const dialog = screen.getByRole('dialog', { name: '配置 API 中转站' });
    const key = within(dialog).getByLabelText('API Key') as HTMLInputElement;
    fireEvent.change(key, { target: { value: 'sk-retry-value' } });
    fireEvent.click(within(dialog).getByRole('button', { name: '保存中转站' }));

    expect(await within(dialog).findByText('relay store unavailable')).toBeTruthy();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    const saveEnabledBeforeRefresh = !(within(dialog).getByRole('button', { name: '保存中转站' }) as HTMLButtonElement).disabled;
    pendingRefresh.reject(new Error('history refresh failed'));
    await waitFor(() => expect(within(dialog).getByText('relay store unavailable')).toBeTruthy());
    expect(key.value).toBe('sk-retry-value');
    expect(saveEnabledBeforeRefresh).toBe(true);
    expect(screen.queryByText('history refresh failed')).toBeNull();
  });

  it('gates writes on the domains they require and exposes the real domain error', async () => {
    const data = dashboardData();
    data.sessions = { status: 'error', error: 'SQLite locked' };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    expect(await screen.findByText('SQLite locked')).toBeTruthy();
    expect((screen.getByRole('button', { name: '立即同步' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '保存当前账号态' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('keeps managed-session mutations available when only the independent sync scan fails', async () => {
    const data = dashboardData();
    data.sessions = { status: 'error', error: 'independent inventory failed' };
    if (data.managedSessions.status !== 'ready') throw new Error('fixture mismatch');
    data.managedSessions.data.sessions = [{
      id: 'thread-a', title: '可管理会话', preview: null, modelProvider: 'openai',
      updatedAt: 1, updatedAtMs: 1000, archived: false, archivedAt: null, scope: 'current',
      current: { home: 'C:\\Users\\alice\\.codex', rolloutPath: 'sessions/thread-a.jsonl', sessionFile: 'sessions/thread-a.jsonl', archived: false, archivedAt: null, updatedAt: 1, updatedAtMs: 1000 },
      shared: null,
    }];
    data.managedSessions.data.totalCount = 1;
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话管理' }));
    fireEvent.click(screen.getByLabelText(/^选择 thread-a/));
    expect((screen.getByRole('button', { name: '立即同步' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '删除所选' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('gates account import on its required files while trusting successful session domains', async () => {
    const data = dashboardData();
    if (data.codexHome.status !== 'ready') throw new Error('fixture must include Codex Home');
    data.codexHome.data.authJson.exists = false;
    data.codexHome.data.stateDb.exists = false;
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    expect((await screen.findByRole('button', { name: '保存当前账号态' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '立即同步' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('keeps relay verification and backup recovery available when Codex Home itself is damaged', async () => {
    const data = dashboardData();
    data.codexHome = { status: 'error', error: 'auth.json is malformed' };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const relay = await screen.findByRole('article', { name: 'API 中转站态' });
    expect((within(relay).getByRole('button', { name: '配置中转站' }) as HTMLButtonElement).disabled).toBe(false);
    expect((within(relay).getByRole('button', { name: '验证连接' }) as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByRole('button', { name: /^恢复此备份/ }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('confirms overwrite when saving an existing account runtime', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '保存当前账号态' }));
    await waitFor(() => expect(apiMocks.importPlusRuntime).toHaveBeenCalledWith(true));
  });

  it('shows sync dry-run before execution and renders the backend receipt', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    apiMocks.syncAllSessions.mockResolvedValue({
      operationId: 'sync-1', backups: [{ backupDir: 'C:\\backups\\sync-1' }],
      insertedThreads: 3, copiedSessionFiles: 2, duplicateThreads: 8,
      skippedMissingSessionFiles: 1, skippedArchivedThreads: 0, mergedSessionIndexEntries: 2,
      warnings: ['审计日志写入失败'],
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '立即同步' }));

    await waitFor(() => expect(apiMocks.dryRunAllSessions).toHaveBeenCalled());
    expect(window.confirm).toHaveBeenCalledWith(expect.stringContaining('预计新增 3 个线程'));
    await waitFor(() => expect(apiMocks.syncAllSessions).toHaveBeenCalled());
    expect(await screen.findByText('操作 ID：sync-1')).toBeTruthy();
    expect(screen.getByText('新增线程：3')).toBeTruthy();
    expect(screen.getByText('备份：1')).toBeTruthy();
    expect(screen.getByText('警告：审计日志写入失败')).toBeTruthy();
    expect(apiMocks.listCodexProcesses).not.toHaveBeenCalled();
  });

  it('keeps the successful sync receipt when only the dashboard refresh fails', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    const pendingRefresh = deferred<DashboardData>();
    const load = vi.fn()
      .mockResolvedValueOnce(dashboardData())
      .mockReturnValueOnce(pendingRefresh.promise);
    apiMocks.syncAllSessions.mockResolvedValue({
      operationId: 'sync-refresh-failed', backups: [], insertedThreads: 1,
      copiedSessionFiles: 1, duplicateThreads: 0, skippedMissingSessionFiles: 0,
      skippedArchivedThreads: 0, mergedSessionIndexEntries: 1,
    });
    render(<App loadDashboard={load} />);
    fireEvent.click(await screen.findByRole('button', { name: '立即同步' }));

    expect(await screen.findByText('操作 ID：sync-refresh-failed')).toBeTruthy();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    const syncEnabledBeforeRefresh = !(screen.getByRole('button', { name: '立即同步' }) as HTMLButtonElement).disabled;
    pendingRefresh.reject(new Error('refresh failed'));
    expect(await screen.findByText(/操作已成功，但状态刷新失败：refresh failed/)).toBeTruthy();
    expect(syncEnabledBeforeRefresh).toBe(true);
  });

  it('refreshes durable history after a failed session sync', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    const pendingRefresh = deferred<DashboardData>();
    const failed = dashboardData();
    if (failed.operations.status !== 'ready') throw new Error('fixture mismatch');
    failed.operations.data = [{
      operationId: 'sync-failed-1', action: 'syncSessions', status: 'rolledBack', phase: 'rollback',
      startedAtMs: 20, completedAtMs: 21, backupDirs: ['C:\\backups\\sync-failed'], counts: {},
    }];
    const load = vi.fn()
      .mockResolvedValueOnce(dashboardData())
      .mockReturnValueOnce(pendingRefresh.promise);
    apiMocks.syncAllSessions.mockRejectedValue(new Error('sync apply failed'));
    render(<App loadDashboard={load} />);
    fireEvent.click(await screen.findByRole('button', { name: '立即同步' }));

    expect(await screen.findByText('sync apply failed')).toBeTruthy();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    const syncEnabledBeforeRefresh = !(screen.getByRole('button', { name: '立即同步' }) as HTMLButtonElement).disabled;
    pendingRefresh.resolve(failed);
    const history = await screen.findByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('sync-failed-1')).toBeTruthy();
    expect(within(history).getByText('已回滚')).toBeTruthy();
    expect(syncEnabledBeforeRefresh).toBe(true);
  });

  it('shows independently restorable backup history with source roots', async () => {
    const data = dashboardData();
    if (data.backups.status !== 'ready') throw new Error('fixture must include backups');
    data.backups.data.push({
      backupDir: 'C:\\backups\\safe-2', sourceRoot: 'C:\\shared-sessions', reason: 'session-sync',
      createdAtMs: 9, fileCount: 8, totalBytes: 8192, verified: true, completeSessions: true,
    });
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    expect(await screen.findByText('session-sync')).toBeTruthy();
    expect(screen.getByText('仅校验并展示最近 5 份备份候选；旧备份不会自动清理。')).toBeTruthy();
    expect(screen.getByText('来源：C:\\shared-sessions')).toBeTruthy();
    expect(screen.getAllByRole('button', { name: /^恢复此备份/ })).toHaveLength(2);
  });

  it('does not conflate loading or failed domains with empty saved state', async () => {
    const data = dashboardData();
    data.runtimes = { status: 'error', error: 'runtime store unavailable' };
    data.backups = { status: 'error', error: 'backup index unavailable' };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const account = await screen.findByRole('article', { name: 'Codex 账号态' });
    expect(within(account).getAllByText('不可用').length).toBeGreaterThan(0);
    const backupPanel = screen.getByRole('complementary', { name: '备份恢复' });
    expect(within(backupPanel).getByText('backup index unavailable')).toBeTruthy();
    expect(within(backupPanel).queryByText('没有可恢复的已验证备份。')).toBeNull();
  });

  it('reports an active mode even when the matching slot has not been saved yet', async () => {
    const data = dashboardData();
    if (data.runtimes.status !== 'ready' || data.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    data.runtimes.data = data.runtimes.data.filter((runtime) => runtime.kind !== 'relay');
    data.runtimeStatus.data = { ...data.runtimeStatus.data, activeRuntimeId: 'relay', confidence: 'mode' };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const relay = await screen.findByRole('article', { name: 'API 中转站态' });
    expect(within(relay).getByText('未保存')).toBeTruthy();
    expect(within(relay).getByText('模式匹配')).toBeTruthy();
  });

  it('renders durable operation history with backup references', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const history = await screen.findByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('history-1')).toBeTruthy();
    expect(within(history).getByText('C:\\backups\\safe-1')).toBeTruthy();
  });

  it('refreshes durable operation history after a failed backend action', async () => {
    const failed = dashboardData();
    if (failed.operations.status !== 'ready') throw new Error('fixture mismatch');
    failed.operations.data = [{
      operationId: 'verify-failed-1', action: 'verifyRelay', status: 'failed', phase: 'verify',
      startedAtMs: 11, completedAtMs: 12, backupDirs: [], counts: {},
    }];
    const load = vi.fn().mockResolvedValueOnce(dashboardData()).mockResolvedValueOnce(failed);
    apiMocks.verifyRelayRuntime.mockRejectedValue(new Error('relay unreachable'));
    render(<App loadDashboard={load} />);

    fireEvent.click(await screen.findByRole('button', { name: '验证连接' }));
    expect(await screen.findByText('relay unreachable')).toBeTruthy();
    const history = await screen.findByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('verify-failed-1')).toBeTruthy();
    expect(within(history).getByText('失败')).toBeTruthy();
  });

  it('closes a running Codex only after confirmation before switching', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('test fixture must include runtimes');
    apiMocks.listCodexProcesses.mockResolvedValue([{ imageName: 'codex.exe', pid: 1234 }]);
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    apiMocks.switchRuntime.mockResolvedValue({
      operationId: 'switch-1', changed: true, runtime: dashboard.runtimes.data[0],
      backups: [], rolledBack: false,
      toShared: { insertedThreads: 0 }, fromShared: { insertedThreads: 0 },
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 Codex 账号' }));

    await waitFor(() => expect(apiMocks.closeCodexProcesses).toHaveBeenCalled());
    expect(apiMocks.switchRuntime).toHaveBeenCalledWith('plus');
  });

  it('closes a running Codex before a confirmed session mutation', async () => {
    const dashboard = dashboardData();
    if (dashboard.managedSessions.status !== 'ready') throw new Error('test fixture must include sessions');
    dashboard.managedSessions.data.sessions = [{
      id: 'thread-a', title: '待删除', preview: null, modelProvider: 'openai',
      updatedAt: 1, updatedAtMs: 1000, archived: true, archivedAt: 1000, scope: 'current',
      current: {
        home: 'C:\\Users\\alice\\.codex', rolloutPath: 'sessions/thread-a.jsonl',
        sessionFile: 'sessions/thread-a.jsonl', archived: true, archivedAt: 1000,
        updatedAt: 1, updatedAtMs: 1000,
      },
      shared: null,
    }];
    dashboard.managedSessions.data.totalCount = 1;
    dashboard.managedSessions.data.archivedCount = 1;
    apiMocks.listCodexProcesses.mockResolvedValue([{ imageName: 'codex.exe', pid: 1234 }]);
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    apiMocks.deleteManagedSessions.mockResolvedValue({
      operationId: 'delete-1', selectedCount: 1, backups: [], deletedThreads: 1,
      deletedSessionFiles: 1, removedSessionIndexEntries: 1, restoredThreads: 0,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话管理' }));
    fireEvent.click(screen.getByLabelText(/^选择 thread-a/));
    fireEvent.click(screen.getByRole('button', { name: '删除所选' }));

    await waitFor(() => expect(apiMocks.closeCodexProcesses).toHaveBeenCalled());
    expect(apiMocks.deleteManagedSessions).toHaveBeenCalledWith(['thread-a'], true);
  });

  it('restores only a verified backup after explicit confirmation', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    apiMocks.restoreBackup.mockResolvedValue({
      operationId: 'restore-1', backupDir: 'C:\\backups\\safe-1', targetRoot: 'C:\\Users\\alice\\.codex',
      restoredFiles: 4, verified: true,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: /^恢复此备份/ }));
    await waitFor(() => expect(apiMocks.restoreBackup).toHaveBeenCalledWith('C:\\backups\\safe-1'));
    expect(await screen.findByText('操作 ID：restore-1')).toBeTruthy();
    expect(screen.getByText('恢复文件：4')).toBeTruthy();
  });
});
