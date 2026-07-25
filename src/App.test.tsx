import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { StrictMode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DashboardData, RuntimeSwitchProgress, RuntimeSwitchResult, UpdateInstallReceipt } from './types';

const apiMocks = vi.hoisted(() => ({
  getAppStatus: vi.fn(),
  getUpdateStartupNotice: vi.fn(),
  checkForUpdates: vi.fn(),
  installUpdate: vi.fn(),
  importPlusRuntime: vi.fn(),
  upsertRelayRuntime: vi.fn(),
  verifyRelayRuntime: vi.fn(),
  listCodexProcesses: vi.fn(),
  closeCodexProcesses: vi.fn(),
  switchRuntime: vi.fn(),
  loadRuntimeDashboard: vi.fn(),
  loadSessionDashboard: vi.fn(),
  loadBackupDashboard: vi.fn(),
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
          id: 'plus', name: 'ChatGPT 账号', kind: 'plus', baseUrl: null, model: 'gpt-5.5',
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
    const initial = dashboardData();
    apiMocks.loadRuntimeDashboard.mockResolvedValue({
      codexHome: initial.codexHome,
      runtimes: initial.runtimes,
      runtimeStatus: initial.runtimeStatus,
      operations: initial.operations,
    });
    apiMocks.loadSessionDashboard.mockResolvedValue({
      sessions: initial.sessions,
      managedSessions: initial.managedSessions,
    });
    apiMocks.loadBackupDashboard.mockResolvedValue({ backups: initial.backups });
    apiMocks.getUpdateStartupNotice.mockResolvedValue(null);
    apiMocks.getAppStatus.mockResolvedValue({
      appName: 'ChatGPT Switch', version: '0.1.5', phase: 'hardened-mvp',
      codexHome: 'C:\\Users\\alice\\.codex',
    });
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.5', updateAvailable: false,
      releaseNotes: null,
      checkedAtMs: 10,
    });
    apiMocks.importPlusRuntime.mockResolvedValue({
      id: 'plus', name: 'ChatGPT 账号', kind: 'plus', baseUrl: null, model: 'gpt-5.5',
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

  it('checks once on startup without blocking the dashboard', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    expect(await screen.findByRole('article', { name: 'ChatGPT 账号态' })).toBeTruthy();
    await waitFor(() => expect(apiMocks.checkForUpdates).toHaveBeenCalledTimes(1));
    expect(screen.getByText('v0.1.5 · 已是最新版')).toBeTruthy();
    expect(screen.queryByRole('region', { name: '发现新版本' })).toBeNull();
  });

  it('does not duplicate the startup check during React StrictMode effect replay', async () => {
    render(<StrictMode><App loadDashboard={() => Promise.resolve(dashboardData())} /></StrictMode>);

    await screen.findByRole('article', { name: 'ChatGPT 账号态' });
    await waitFor(() => expect(apiMocks.checkForUpdates).toHaveBeenCalledTimes(1));
  });

  it('shows a dismissible non-blocking update banner and installs with one click', async () => {
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: '<img src=x onerror=alert(1)> 安全修复',
      checkedAtMs: 10,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const banner = await screen.findByRole('region', { name: '发现新版本' });
    expect(within(banner).getByText('<img src=x onerror=alert(1)> 安全修复')).toBeTruthy();
    apiMocks.installUpdate.mockResolvedValue({
      fromVersion: '0.1.5', toVersion: '0.1.6', downloadedBytes: 100,
      sha256: 'a'.repeat(64), restarting: true,
    });
    fireEvent.click(within(banner).getByRole('button', { name: '立即更新' }));
    await waitFor(() => expect(apiMocks.installUpdate).toHaveBeenCalledTimes(1));
    expect(await screen.findByText('v0.1.6 已下载并校验，正在重启完成更新…')).toBeTruthy();
    expect((within(banner).getByRole('button', { name: '正在下载并安装…' }) as HTMLButtonElement).disabled).toBe(true);

    expect((within(banner).getByRole('button', { name: '关闭更新提示' }) as HTMLButtonElement).disabled).toBe(true);
    expect(apiMocks.checkForUpdates).toHaveBeenCalledTimes(1);
  });

  it('keeps the update available for retry when installation fails', async () => {
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: null, checkedAtMs: 10,
    });
    apiMocks.installUpdate.mockRejectedValue(new Error('digest mismatch'));
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const install = await screen.findByRole('button', { name: '立即更新' });
    fireEvent.click(install);
    expect(await screen.findByText('digest mismatch')).toBeTruthy();
    await waitFor(() => expect((screen.getByRole('button', { name: '立即更新' }) as HTMLButtonElement).disabled).toBe(false));
  });

  it('disables local mutations while update installation is pending', async () => {
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: null, checkedAtMs: 10,
    });
    const pending = deferred<{
      fromVersion: string; toVersion: string; downloadedBytes: number; sha256: string; restarting: boolean;
    }>();
    apiMocks.installUpdate.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '立即更新' }));
    await waitFor(() => expect(apiMocks.installUpdate).toHaveBeenCalledTimes(1));
    const saveAccount = screen.getByRole('button', { name: '保存当前账号态' }) as HTMLButtonElement;
    const sync = screen.getByRole('button', { name: '立即同步' }) as HTMLButtonElement;
    expect(saveAccount.disabled).toBe(true);
    expect(sync.disabled).toBe(true);
    fireEvent.click(saveAccount);
    expect(apiMocks.importPlusRuntime).not.toHaveBeenCalled();

    pending.reject(new Error('injected update failure'));
    expect(await screen.findByText('injected update failure')).toBeTruthy();
  });

  it('ignores a stale switch click while update installation is pending', async () => {
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: null, checkedAtMs: 10,
    });
    const pending = deferred<{
      fromVersion: string; toVersion: string; downloadedBytes: number; sha256: string; restarting: boolean;
    }>();
    apiMocks.installUpdate.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const switchButton = await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }) as HTMLButtonElement;
    fireEvent.click(await screen.findByRole('button', { name: '立即更新' }));
    await waitFor(() => expect(switchButton.disabled).toBe(true));

    switchButton.disabled = false;
    fireEvent.click(switchButton);

    expect(apiMocks.switchRuntime).not.toHaveBeenCalled();
    expect(screen.queryByRole('region', { name: '运行态切换进度' })).toBeNull();
    pending.reject(new Error('injected update failure'));
    await screen.findByText('injected update failure');
  });

  it('blocks backup retry while update installation is pending', async () => {
    const data = dashboardData();
    data.backups = { status: 'error', error: 'backup index unavailable' };
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: null, checkedAtMs: 10,
    });
    const pending = deferred<UpdateInstallReceipt>();
    apiMocks.installUpdate.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const retry = await screen.findByRole('button', { name: '重试' }) as HTMLButtonElement;
    fireEvent.click(await screen.findByRole('button', { name: '立即更新' }));
    await waitFor(() => expect(retry.disabled).toBe(true));

    retry.disabled = false;
    fireEvent.click(retry);

    expect(apiMocks.loadBackupDashboard).not.toHaveBeenCalled();
    pending.reject(new Error('injected update failure'));
    await screen.findByText('injected update failure');
  });

  it('disables update installation while a local mutation is pending', async () => {
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: null, checkedAtMs: 10,
    });
    const pending = deferred<{
      id: string; name: string; kind: 'plus'; baseUrl: null; model: string;
      createdAtMs: number; lastUsedAtMs: null; lastVerifiedAtMs: null;
    }>();
    apiMocks.importPlusRuntime.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const install = await screen.findByRole('button', { name: '立即更新' }) as HTMLButtonElement;
    fireEvent.click(screen.getByRole('button', { name: '保存当前账号态' }));
    fireEvent.click(await screen.findByRole('button', { name: '确认覆盖' }));
    await waitFor(() => expect(apiMocks.importPlusRuntime).toHaveBeenCalledTimes(1));
    expect(install.disabled).toBe(true);
    fireEvent.click(install);
    expect(apiMocks.installUpdate).not.toHaveBeenCalled();

    pending.resolve({
      id: 'plus', name: 'ChatGPT 账号', kind: 'plus', baseUrl: null, model: 'gpt-5.5',
      createdAtMs: 1, lastUsedAtMs: null, lastVerifiedAtMs: null,
    });
    await waitFor(() => expect(install.disabled).toBe(false));
  });

  it('shows completion and rollback notices from the restarted process', async () => {
    apiMocks.getUpdateStartupNotice.mockResolvedValue({ status: 'updated' });
    const { unmount } = render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    expect(await screen.findByText('更新完成，已启动新版本。')).toBeTruthy();
    unmount();

    apiMocks.getUpdateStartupNotice.mockResolvedValue({ status: 'rolledBack' });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    expect(await screen.findByText('更新启动失败，已恢复并重新启动旧版本。')).toBeTruthy();
  });

  it('keeps startup failures silent but reports a manual check failure', async () => {
    apiMocks.checkForUpdates
      .mockRejectedValueOnce(new Error('startup offline'))
      .mockRejectedValueOnce(new Error('manual offline'));
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    await waitFor(() => expect(apiMocks.checkForUpdates).toHaveBeenCalledTimes(1));
    const button = screen.getByRole('button', { name: '检查更新' }) as HTMLButtonElement;
    await waitFor(() => expect(button.disabled).toBe(false));
    expect(screen.queryByText('startup offline')).toBeNull();

    fireEvent.click(button);
    expect(await screen.findByText('manual offline')).toBeTruthy();
    expect(apiMocks.checkForUpdates).toHaveBeenCalledTimes(2);
  });

  it('disables manual checks while the startup check is pending', async () => {
    const pending = deferred<{
      currentVersion: string; latestVersion: string; updateAvailable: boolean;
      releaseNotes: null; checkedAtMs: number;
    }>();
    apiMocks.checkForUpdates.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const button = await screen.findByRole('button', { name: '检查中' }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    fireEvent.click(button);
    expect(apiMocks.checkForUpdates).toHaveBeenCalledTimes(1);

    pending.resolve({
      currentVersion: '0.1.5', latestVersion: '0.1.5', updateAvailable: false,
      releaseNotes: null,
      checkedAtMs: 10,
    });
    await waitFor(() => expect(screen.getByRole('button', { name: '检查更新' })).toBeTruthy());
  });

  it('renders saved, current, and verified as separate runtime states', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const account = await screen.findByRole('article', { name: 'ChatGPT 账号态' });
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
    await screen.findByRole('article', { name: 'ChatGPT 账号态' });
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
    const trigger = await screen.findByRole('button', { name: '配置中转站' });
    trigger.focus();
    fireEvent.click(trigger);

    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    await waitFor(() => expect(document.activeElement).toBe(
      within(panel).getByRole('heading', { name: '配置 API 中转站' }),
    ));
    const key = within(panel).getByLabelText('API Key') as HTMLInputElement;
    expect(key.type).toBe('password');
    fireEvent.change(within(panel).getByLabelText('Base URL'), { target: { value: 'https://new.example.com/v1' } });
    fireEvent.change(within(panel).getByLabelText('模型'), { target: { value: 'gpt-5.5-mini' } });
    fireEvent.change(key, { target: { value: 'sk-secret' } });
    fireEvent.click(within(panel).getByRole('button', { name: '保存中转站' }));

    await waitFor(() => expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith({
      baseUrl: 'https://new.example.com/v1', model: 'gpt-5.5-mini', apiKey: 'sk-secret',
    }));
    expect(prompt).not.toHaveBeenCalled();
    expect(screen.queryByText('sk-secret')).toBeNull();
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  it('restores relay configuration focus after inline cancellation', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    const trigger = await screen.findByRole('button', { name: '配置中转站' });
    trigger.focus();
    fireEvent.click(trigger);

    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    const heading = within(panel).getByRole('heading', { name: '配置 API 中转站' });
    await waitFor(() => expect(document.activeElement).toBe(heading));
    fireEvent.click(within(panel).getByRole('button', { name: '取消' }));

    await waitFor(() => expect(screen.queryByRole('region', { name: '配置 API 中转站' })).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  it('submits an empty key explicitly to preserve the existing encrypted relay key', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));

    fireEvent.click(within(screen.getByRole('region', { name: '配置 API 中转站' }))
      .getByRole('button', { name: '保存中转站' }));

    await waitFor(() => expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith({
      baseUrl: 'https://relay.example.com/v1', model: 'gpt-5.5', apiKey: '',
    }));
  });

  it('rejects relay URLs with embedded credentials before invoking the backend', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    fireEvent.change(within(panel).getByLabelText('Base URL'), {
      target: { value: 'https://user:secret@relay.example.com/v1' },
    });
    fireEvent.click(within(panel).getByRole('button', { name: '保存中转站' }));

    expect(await within(panel).findByText('Base URL 不能包含用户名、密码、查询参数或片段')).toBeTruthy();
    expect(apiMocks.upsertRelayRuntime).not.toHaveBeenCalled();
  });

  it('normalizes a relay host without a scheme to https before saving', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    fireEvent.change(within(panel).getByLabelText('Base URL'), { target: { value: 'relay.example.com/v1' } });
    fireEvent.click(within(panel).getByRole('button', { name: '保存中转站' }));

    await waitFor(() => expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith(expect.objectContaining({
      baseUrl: 'https://relay.example.com/v1',
    })));
  });

  it('rejects non-local plain HTTP relays without opening a browser confirmation', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    fireEvent.change(within(panel).getByLabelText('Base URL'), { target: { value: 'http://relay.example.com/v1' } });
    expect(within(panel).getByRole('alert').textContent).toContain('远程中转站必须使用 HTTPS');
    expect((within(panel).getByRole('button', { name: '保存中转站' }) as HTMLButtonElement).disabled).toBe(true);

    expect(confirm).not.toHaveBeenCalled();
    expect(apiMocks.upsertRelayRuntime).not.toHaveBeenCalled();
  });

  it('allows loopback HTTP relays without a browser confirmation', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    fireEvent.change(within(panel).getByLabelText('Base URL'), { target: { value: 'http://127.0.0.1:8787/v1' } });
    fireEvent.change(within(panel).getByLabelText('API Key'), { target: { value: 'sk-loopback' } });
    fireEvent.click(within(panel).getByRole('button', { name: '保存中转站' }));

    await waitFor(() => expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith(expect.objectContaining({
      baseUrl: 'http://127.0.0.1:8787/v1',
    })));
    expect(confirm).not.toHaveBeenCalled();
  });

  it('keeps the successful receipt and closes the dialog when only refresh fails', async () => {
    const pendingRefresh = deferred<{
      codexHome: DashboardData['codexHome'];
      runtimes: DashboardData['runtimes'];
      runtimeStatus: DashboardData['runtimeStatus'];
      operations: DashboardData['operations'];
    }>();
    apiMocks.loadRuntimeDashboard.mockReturnValueOnce(pendingRefresh.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    fireEvent.click(within(screen.getByRole('region', { name: '配置 API 中转站' }))
      .getByRole('button', { name: '保存中转站' }));

    expect(await screen.findByText('API 中转站已保存')).toBeTruthy();
    await waitFor(() => expect(apiMocks.loadRuntimeDashboard).toHaveBeenCalledTimes(1));
    const panelClosedBeforeRefresh = screen.queryByRole('region', { name: '配置 API 中转站' }) === null;
    const configureEnabledBeforeRefresh = !(screen.getByRole('button', { name: '配置中转站' }) as HTMLButtonElement).disabled;
    pendingRefresh.reject(new Error('refresh failed'));
    expect(await screen.findByText(/操作已成功，但状态刷新失败：refresh failed/)).toBeTruthy();
    expect(panelClosedBeforeRefresh).toBe(true);
    expect(configureEnabledBeforeRefresh).toBe(true);
  });

  it('keeps the relay key and shows backend save failures inside the dialog', async () => {
    const pendingRefresh = deferred<{
      codexHome: DashboardData['codexHome'];
      runtimes: DashboardData['runtimes'];
      runtimeStatus: DashboardData['runtimeStatus'];
      operations: DashboardData['operations'];
    }>();
    apiMocks.upsertRelayRuntime.mockRejectedValue(new Error('relay store unavailable'));
    apiMocks.loadRuntimeDashboard.mockReturnValueOnce(pendingRefresh.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    const key = within(panel).getByLabelText('API Key') as HTMLInputElement;
    fireEvent.change(key, { target: { value: 'sk-retry-value' } });
    fireEvent.click(within(panel).getByRole('button', { name: '保存中转站' }));

    expect(await within(panel).findByText('relay store unavailable')).toBeTruthy();
    await waitFor(() => expect(apiMocks.loadRuntimeDashboard).toHaveBeenCalledTimes(1));
    const saveEnabledBeforeRefresh = !(within(panel).getByRole('button', { name: '保存中转站' }) as HTMLButtonElement).disabled;
    pendingRefresh.reject(new Error('history refresh failed'));
    await waitFor(() => expect(within(panel).getByText('relay store unavailable')).toBeTruthy());
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

    fireEvent.click(await screen.findByRole('button', { name: '会话' }));
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
    const confirm = vi.spyOn(window, 'confirm');
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '保存当前账号态' }));
    const confirmation = await screen.findByRole('region', { name: '覆盖已保存的 ChatGPT 账号态' });
    expect(within(confirmation).getByText('当前账号态会先归档，再写入新的加密快照。')).toBeTruthy();
    fireEvent.click(within(confirmation).getByRole('button', { name: '确认覆盖' }));
    await waitFor(() => expect(apiMocks.importPlusRuntime).toHaveBeenCalledWith(true));
    expect(confirm).not.toHaveBeenCalled();
  });

  it('shows sync dry-run before execution and renders the backend receipt', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    apiMocks.syncAllSessions.mockResolvedValue({
      operationId: 'sync-1', backups: [{ backupDir: 'C:\\backups\\sync-1' }],
      insertedThreads: 3, copiedSessionFiles: 2, duplicateThreads: 8,
      skippedMissingSessionFiles: 1, skippedArchivedThreads: 0, mergedSessionIndexEntries: 2,
      warnings: ['审计日志写入失败'],
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '立即同步' }));

    await waitFor(() => expect(apiMocks.dryRunAllSessions).toHaveBeenCalled());
    const confirmation = await screen.findByRole('region', { name: '会话同步预检已完成' });
    expect(within(confirmation).getByText('新增 3 个线程')).toBeTruthy();
    fireEvent.click(within(confirmation).getByRole('button', { name: '开始同步' }));
    await waitFor(() => expect(apiMocks.syncAllSessions).toHaveBeenCalled());
    expect(await screen.findByText('操作 ID：sync-1')).toBeTruthy();
    expect(screen.getByText('新增线程：3')).toBeTruthy();
    expect(screen.getByText('备份：1')).toBeTruthy();
    expect(screen.getByText('警告：审计日志写入失败')).toBeTruthy();
    expect(apiMocks.listCodexProcesses).not.toHaveBeenCalled();
    expect(confirm).not.toHaveBeenCalled();
  });

  it('keeps the successful sync receipt when only the dashboard refresh fails', async () => {
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
    fireEvent.click(await screen.findByRole('button', { name: '开始同步' }));

    expect(await screen.findByText('操作 ID：sync-refresh-failed')).toBeTruthy();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    const syncEnabledBeforeRefresh = !(screen.getByRole('button', { name: '立即同步' }) as HTMLButtonElement).disabled;
    pendingRefresh.reject(new Error('refresh failed'));
    expect(await screen.findByText(/操作已成功，但状态刷新失败：refresh failed/)).toBeTruthy();
    expect(syncEnabledBeforeRefresh).toBe(true);
  });

  it('refreshes durable history after a failed session sync', async () => {
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
    fireEvent.click(await screen.findByRole('button', { name: '开始同步' }));

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
    expect(screen.getByText('最近 5 份已验证快照')).toBeTruthy();
    expect(screen.getByText('来源：C:\\shared-sessions')).toBeTruthy();
    expect(screen.getAllByRole('button', { name: /^恢复此备份/ })).toHaveLength(2);
  });

  it('does not conflate loading or failed domains with empty saved state', async () => {
    const data = dashboardData();
    data.runtimes = { status: 'error', error: 'runtime store unavailable' };
    data.backups = { status: 'error', error: 'backup index unavailable' };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const account = await screen.findByRole('article', { name: 'ChatGPT 账号态' });
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
    apiMocks.loadRuntimeDashboard.mockResolvedValueOnce({
      codexHome: failed.codexHome,
      runtimes: failed.runtimes,
      runtimeStatus: failed.runtimeStatus,
      operations: failed.operations,
    });
    apiMocks.verifyRelayRuntime.mockRejectedValue(new Error('relay unreachable'));
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '验证连接' }));
    expect(await screen.findByText('relay unreachable')).toBeTruthy();
    const history = await screen.findByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('verify-failed-1')).toBeTruthy();
    expect(within(history).getByText('失败')).toBeTruthy();
  });

  it('shows switch loading immediately, streams stages, and keeps the task across tabs', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('test fixture must include runtimes');
    const pendingSwitch = deferred<{
      operationId: string;
      changed: boolean;
      runtime: (typeof dashboard.runtimes.data)[number];
      backups: [];
      rolledBack: boolean;
      toShared: { insertedThreads: number };
      fromShared: { insertedThreads: number };
    }>();
    let onProgress!: (event: {
      phase: 'detectingApp' | 'closingApp' | 'backingUpCurrent' | 'complete';
      timestampMs: number;
    }) => void;
    apiMocks.switchRuntime.mockImplementation((_runtimeId, callback) => {
      onProgress = callback;
      return pendingSwitch.promise;
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));

    const progress = screen.getByRole('region', { name: '运行态切换进度' });
    expect(within(progress).getByRole('heading', { name: '正在切换到ChatGPT 账号' })).toBeTruthy();
    expect(screen.getByText('切换 ChatGPT 账号处理中')).toBeTruthy();
    expect(apiMocks.switchRuntime).toHaveBeenCalledWith('plus', expect.any(Function));
    expect(apiMocks.listCodexProcesses).not.toHaveBeenCalled();
    expect(apiMocks.closeCodexProcesses).not.toHaveBeenCalled();

    act(() => {
      onProgress({ phase: 'detectingApp', timestampMs: 100 });
      onProgress({ phase: 'closingApp', timestampMs: 110 });
      onProgress({ phase: 'backingUpCurrent', timestampMs: 120 });
    });
    expect(within(progress).getByText('备份当前数据', { selector: 'strong' }).closest('li')?.className).toBe('active');
    expect(within(progress).getByText('检测 ChatGPT', { selector: 'strong' }).closest('li')?.className).toBe('done');

    fireEvent.click(screen.getByRole('button', { name: '技能' }));
    expect(screen.getByRole('region', { name: '运行态切换进度' })).toBeTruthy();

    act(() => onProgress({ phase: 'complete', timestampMs: 130 }));
    pendingSwitch.resolve({
      operationId: 'switch-1', changed: true, runtime: dashboard.runtimes.data[0],
      backups: [], rolledBack: false,
      toShared: { insertedThreads: 0 }, fromShared: { insertedThreads: 0 },
    });
    expect(await screen.findByText('切换完成，可以重新打开 ChatGPT。会话索引将在打开会话页时刷新。')).toBeTruthy();
  });

  it('serializes same-tick switch clicks and waits for command completion before scanning stale sessions', async () => {
    const dashboard = dashboardData();
    dashboard.backups = { status: 'error', error: 'backup index unavailable' };
    if (dashboard.runtimes.status !== 'ready') throw new Error('test fixture must include runtimes');
    const pendingSwitch = deferred<RuntimeSwitchResult>();
    let onProgress!: (event: RuntimeSwitchProgress) => void;
    apiMocks.switchRuntime.mockImplementation((_runtimeId, callback) => {
      onProgress = callback;
      return pendingSwitch.promise;
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    const switchButton = await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }) as HTMLButtonElement;
    act(() => {
      switchButton.click();
      switchButton.click();
    });
    expect(apiMocks.switchRuntime).toHaveBeenCalledTimes(1);

    act(() => onProgress({ phase: 'applyingRuntime', timestampMs: 110 }));
    fireEvent.click(screen.getByRole('button', { name: '会话' }));
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();

    act(() => onProgress({ phase: 'complete', timestampMs: 120 }));
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();

    pendingSwitch.resolve({
      operationId: 'switch-serialized',
      changed: true,
      runtime: dashboard.runtimes.data[0],
      backups: [],
      rolledBack: false,
      toShared: {
        insertedThreads: 0,
        copiedSessionFiles: 0,
        duplicateThreads: 0,
        skippedMissingSessionFiles: 0,
        skippedArchivedThreads: 0,
        mergedSessionIndexEntries: 0,
      },
      fromShared: {
        insertedThreads: 0,
        copiedSessionFiles: 0,
        duplicateThreads: 0,
        skippedMissingSessionFiles: 0,
        skippedArchivedThreads: 0,
        mergedSessionIndexEntries: 0,
      },
    });
    await waitFor(() => expect(apiMocks.loadSessionDashboard).toHaveBeenCalledTimes(1));
  });

  it('marks backups stale after the current snapshot even when switching fails before mutation', async () => {
    const pendingSwitch = deferred<RuntimeSwitchResult>();
    let onProgress!: (event: RuntimeSwitchProgress) => void;
    apiMocks.switchRuntime.mockImplementation((_runtimeId, callback) => {
      onProgress = callback;
      return pendingSwitch.promise;
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));
    act(() => onProgress({ phase: 'backingUpCurrent', timestampMs: 100 }));
    expect(screen.queryByRole('button', { name: '加载备份' })).toBeNull();

    act(() => onProgress({ phase: 'backingUpShared', timestampMs: 110 }));
    const loadBackups = screen.getByRole('button', { name: '加载备份' }) as HTMLButtonElement;
    expect(loadBackups.disabled).toBe(true);

    act(() => {
      onProgress({
        phase: 'failed',
        timestampMs: 120,
        message: 'shared backup failed',
        outcome: 'failedBeforeWrite',
      });
      pendingSwitch.reject(new Error('shared backup failed'));
    });
    await screen.findAllByText('shared backup failed');
    await waitFor(() => expect(loadBackups.disabled).toBe(false));

    fireEvent.click(loadBackups);
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1));
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();
  });

  it('loads runtime state first, then sessions and backups only on demand', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('test fixture must include runtimes');
    const plusRuntime = dashboard.runtimes.data[0];
    apiMocks.switchRuntime.mockImplementation(async (_runtimeId, onProgress) => {
      onProgress({ phase: 'detectingApp', timestampMs: 100 });
      onProgress({ phase: 'applyingRuntime', timestampMs: 110 });
      onProgress({ phase: 'complete', timestampMs: 120 });
      return {
        operationId: 'switch-lazy', changed: true, runtime: plusRuntime,
        backups: [], rolledBack: false,
        toShared: { insertedThreads: 0 }, fromShared: { insertedThreads: 0 },
      };
    });

    render(<App />);

    await screen.findByRole('article', { name: 'ChatGPT 账号态' });
    expect(apiMocks.loadRuntimeDashboard).toHaveBeenCalledTimes(1);
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();
    expect(apiMocks.loadBackupDashboard).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: '切换到 ChatGPT 账号' }));
    await waitFor(() => expect(apiMocks.loadRuntimeDashboard).toHaveBeenCalledTimes(2));
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();
    expect(apiMocks.loadBackupDashboard).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: '会话' }));
    await waitFor(() => expect(apiMocks.loadSessionDashboard).toHaveBeenCalledTimes(1));
    expect(apiMocks.loadBackupDashboard).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: '运行态' }));
    fireEvent.click(await screen.findByRole('button', { name: '加载备份' }));
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1));
  });

  it('infers the interrupted step from the phase before rollback', async () => {
    apiMocks.switchRuntime.mockImplementation(async (_runtimeId, onProgress) => {
      onProgress({ phase: 'detectingApp', timestampMs: 100 });
      onProgress({ phase: 'applyingRuntime', timestampMs: 110 });
      onProgress({ phase: 'rollingBack', timestampMs: 120 });
      onProgress({
        phase: 'failed',
        timestampMs: 130,
        outcome: 'rolledBack',
        message: 'runtime apply failed',
      });
      throw new Error('runtime apply failed');
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));

    const progress = await screen.findByRole('region', { name: '运行态切换进度' });
    await waitFor(() => expect(within(progress).getByText('已恢复切换前状态')).toBeTruthy());
    const interrupted = within(progress).getByText('应用运行态', { selector: 'strong' }).closest('li');
    expect(interrupted?.className).toBe('failed');
    expect(within(interrupted as HTMLElement).getByText('中断')).toBeTruthy();
    expect(within(progress).queryByText('正在恢复切换前状态')).toBeNull();
  });

  it('closes a running ChatGPT process only after inline session-delete confirmation', async () => {
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
    apiMocks.listCodexProcesses.mockResolvedValue([{ imageName: 'ChatGPT.exe', pid: 1234, parentPid: 10 }]);
    const confirm = vi.spyOn(window, 'confirm');
    apiMocks.deleteManagedSessions.mockResolvedValue({
      operationId: 'delete-1', selectedCount: 1, backups: [], deletedThreads: 1,
      deletedSessionFiles: 1, removedSessionIndexEntries: 1, restoredThreads: 0,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话' }));
    fireEvent.click(screen.getByLabelText(/^选择 thread-a/));
    fireEvent.click(screen.getByRole('button', { name: '删除所选' }));
    expect(apiMocks.deleteManagedSessions).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: '确认删除' }));

    await waitFor(() => expect(apiMocks.closeCodexProcesses).toHaveBeenCalled());
    expect(apiMocks.deleteManagedSessions).toHaveBeenCalledWith(['thread-a'], true);
    expect(confirm).not.toHaveBeenCalled();
  });

  it('restores only a verified backup after explicit confirmation', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    apiMocks.restoreBackup.mockResolvedValue({
      operationId: 'restore-1', backupDir: 'C:\\backups\\safe-1', targetRoot: 'C:\\Users\\alice\\.codex',
      restoredFiles: 4, verified: true,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: /^恢复此备份/ }));
    expect(apiMocks.restoreBackup).not.toHaveBeenCalled();
    const confirmation = screen.getByRole('region', { name: '恢复已验证备份' });
    fireEvent.click(within(confirmation).getByRole('button', { name: '开始恢复' }));
    await waitFor(() => expect(apiMocks.restoreBackup).toHaveBeenCalledWith('C:\\backups\\safe-1'));
    expect(await screen.findByText('操作 ID：restore-1')).toBeTruthy();
    expect(screen.getByText('恢复文件：4')).toBeTruthy();
    expect(confirm).not.toHaveBeenCalled();
  });

  it('uses only inline interaction surfaces and never invokes native dialogs', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    const prompt = vi.spyOn(window, 'prompt');
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    expect(screen.getByRole('region', { name: '配置 API 中转站' })).toBeTruthy();
    expect(document.querySelector('dialog')).toBeNull();
    expect(screen.queryByRole('dialog')).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: '取消' }));
    fireEvent.click(screen.getByRole('button', { name: '保存当前账号态' }));
    expect(screen.getByRole('region', { name: '覆盖已保存的 ChatGPT 账号态' })).toBeTruthy();
    expect(confirm).not.toHaveBeenCalled();
    expect(prompt).not.toHaveBeenCalled();
  });
});
