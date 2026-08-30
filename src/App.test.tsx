import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { StrictMode } from 'react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type {
  BackupDeleteReceipt,
  BackupDashboardData,
  CheckpointCleanupReceipt,
  CreateFullBackupReceipt,
  DashboardData,
  RuntimeDashboardData,
  RuntimeSwitchProgress,
  RuntimeSwitchResult,
  ShadowScanReport,
  UpdateInstallReceipt,
} from './types';

const apiMocks = vi.hoisted(() => ({
  getAppStatus: vi.fn(),
  getDiagnosticStatus: vi.fn(),
  exportDiagnostics: vi.fn(),
  retryDiagnosticExport: vi.fn(),
  openDiagnosticExport: vi.fn(),
  openDiagnosticLogDirectory: vi.fn(),
  clearDiagnosticLogs: vi.fn(),
  recordFrontendDiagnostic: vi.fn(),
  requestAppExit: vi.fn(),
  getUpdateStartupNotice: vi.fn(),
  checkForUpdates: vi.fn(),
  installUpdate: vi.fn(),
  cleanupAutomaticCheckpoints: vi.fn(),
  createFullBackup: vi.fn(),
  deleteBackup: vi.fn(),
  importPlusRuntime: vi.fn(),
  upsertRelayRuntime: vi.fn(),
  getMobileContinuityStatus: vi.fn(),
  setMobileContinuityEnabled: vi.fn(),
  acknowledgeMobileContinuityNotice: vi.fn(),
  publishMobileContinuitySession: vi.fn(),
  listCodexProcesses: vi.fn(),
  closeCodexProcesses: vi.fn(),
  launchChatgpt: vi.fn(),
  switchRuntime: vi.fn(),
  loadRuntimeDashboard: vi.fn(),
  loadSessionDashboard: vi.fn(),
  loadBackupDashboard: vi.fn(),
  mergeAndRepairSessions: vi.fn(),
  restoreSessionsVisible: vi.fn(),
  scanSessionStorage: vi.fn(),
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

function shadowScanReport(): ShadowScanReport {
  return {
    schemaVersion: 1,
    scanId: 'session-storage-shadow-1',
    generatedAtMs: 1_786_400_000_000,
    status: 'reviewRequired',
    migrationRequired: true,
    deletionEnabled: false,
    summary: {
      schemaVersion: 1,
      onlineScanOnly: true,
      nonAtomicAcrossDatabases: true,
      logicalSessionCount: 995,
      canonicalCandidateCount: 945,
      duplicatedSessionCount: 751,
      conflictSessionCount: 50,
      highConfidenceCopyCount: 701,
      sessionFileCount: 3_610,
      sessionBytes: 7_068_846_886,
      potentialReclaimBytes: 3_000_000_000,
      markerFileCount: 2_349,
      runtimeDatabaseCount: 3,
      backupDatabaseCount: 0,
      runtimeReferenceCount: 3_182,
      missingRuntimeReferenceCount: 0,
      mismatchedRuntimeReferenceCount: 0,
      cacheHitCount: 3_000,
      cacheMissCount: 610,
      stableFileCount: 3_000,
      turnContextCount: 4_000,
      resolvedTurnProvenanceCount: 500,
      historicalUnknownTurnCount: 3_500,
      incompleteTurnProvenanceCount: 0,
      relationCounts: {
        equal: 1,
        equalExceptProvider: 696,
        prefix: 4,
        divergent: 50,
        unknown: 0,
      },
    },
    issues: [{ code: 'divergentSession', count: 50 }],
  };
}

function canonicalReadyShadowScanReport(): ShadowScanReport {
  const report = shadowScanReport();
  return {
    ...report,
    status: 'canonicalReady',
    migrationRequired: false,
    summary: {
      ...report.summary,
      duplicatedSessionCount: 0,
      conflictSessionCount: 0,
      highConfidenceCopyCount: 0,
      potentialReclaimBytes: 0,
      markerFileCount: 0,
      relationCounts: {
        equal: 0,
        equalExceptProvider: 0,
        prefix: 0,
        divergent: 0,
        unknown: 0,
      },
    },
    issues: [],
  };
}

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
        authSummary: { authMode: 'chatgpt', topLevelKeys: ['auth_mode', 'tokens'], hasTokensObject: true },
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
    sessionStorage: { status: 'ready', data: canonicalReadyShadowScanReport() },
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
    backupStorage: {
      status: 'ready',
      data: {
        totalCount: 19,
        totalBytes: 4_165_388_250,
        reclaimableCount: 2,
        reclaimableBytes: 1_471_410_293,
        retainedCount: 17,
        warnings: [],
        lastCleanup: null,
      },
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

function retainReactClickHandler(element: HTMLElement) {
  const propsKey = Object.keys(element).find((key) => key.startsWith('__reactProps$'));
  const props = propsKey
    ? (element as unknown as Record<string, { onClick?: () => void }>)[propsKey]
    : undefined;
  if (typeof props?.onClick !== 'function') throw new Error('React click handler is unavailable');
  return props.onClick;
}

describe('App release-hardening UI', () => {
  beforeEach(() => {
    for (const mock of Object.values(apiMocks)) {
      mock.mockReset();
      mock.mockResolvedValue(undefined);
    }
    apiMocks.listCodexProcesses.mockResolvedValue([]);
    apiMocks.requestAppExit.mockResolvedValue({ scheduled: true });
    apiMocks.launchChatgpt.mockResolvedValue({ status: 'alreadyRunning', message: null });
    apiMocks.listSkills.mockResolvedValue([]);
    apiMocks.getMobileContinuityStatus.mockResolvedValue({
      enabled: true,
      noticePending: false,
      initializedAtMs: 1,
      queued: 0,
      publishing: 0,
      remotePublished: 0,
      partial: 0,
      conflict: 0,
      needsManual: 0,
      items: [],
    });
    apiMocks.getDiagnosticStatus.mockResolvedValue({
      available: true,
      eventCount: 0,
      totalBytes: 0,
      retentionDays: 7,
      maxBytes: 10 * 1024 * 1024,
      oldestEventAtMs: null,
      newestEventAtMs: null,
      warnings: [],
    });
    const initial = dashboardData();
    apiMocks.loadRuntimeDashboard.mockResolvedValue({
      codexHome: initial.codexHome,
      sessionStorage: initial.sessionStorage,
      runtimes: initial.runtimes,
      runtimeStatus: initial.runtimeStatus,
      operations: initial.operations,
    });
    apiMocks.loadSessionDashboard.mockResolvedValue({
      sessions: initial.sessions,
      managedSessions: initial.managedSessions,
      sessionStorage: initial.sessionStorage,
    });
    apiMocks.loadBackupDashboard.mockResolvedValue({
      backups: initial.backups,
      backupStorage: initial.backupStorage,
      operations: initial.operations,
    });
    apiMocks.cleanupAutomaticCheckpoints.mockResolvedValue({
      operationId: 'cleanup-default',
      attemptedCount: 2,
      failedCount: 0,
      reclaimedCount: 2,
      reclaimedBytes: 1_471_410_293,
      retainedCount: 17,
      warnings: [],
    });
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
    apiMocks.createFullBackup.mockResolvedValue({
      operationId: 'backup-default',
      backups: [
        {
          backupDir: 'C:\\backups\\manual-current-default',
          sourceRoot: 'C:\\Users\\alice\\.codex',
          reason: 'manual-full-backup',
          createdAtMs: 20,
          scope: 'full',
          trackedDatabaseCount: 4,
          completeSessions: true,
        },
        {
          backupDir: 'C:\\backups\\manual-shared-default',
          sourceRoot: 'C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\shared-sessions',
          reason: 'manual-full-backup',
          createdAtMs: 21,
          scope: 'full',
          trackedDatabaseCount: 4,
          completeSessions: true,
        },
      ],
      warnings: [],
    });
    apiMocks.deleteBackup.mockResolvedValue({
      operationId: 'delete-backup-default',
      backupDir: 'C:\\backups\\safe-1',
      reclaimedBytes: 4096,
      warnings: [],
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

  it('renders the scan-only storage contract and refreshes the typed shadow report', async () => {
    const refreshed = shadowScanReport();
    refreshed.status = 'migrationAvailable';
    refreshed.summary.conflictSessionCount = 0;
    refreshed.issues = [];
    apiMocks.scanSessionStorage.mockResolvedValue(refreshed);
    const data = dashboardData();
    data.sessionStorage = { status: 'ready', data: shadowScanReport() };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const panel = await screen.findByRole('complementary', { name: '会话存储状态' });
    expect(within(panel).getByText('发现冲突或异常，需先复核', { exact: false })).toBeTruthy();
    expect(within(panel).getByText('在线仅扫描，不删除', { exact: false })).toBeTruthy();
    expect(within(panel).getByText('回合来源已解析 500/4000', { exact: false })).toBeTruthy();
    expect(within(panel).getByText('3500 个迁移前回合来源未知', { exact: false })).toBeTruthy();

    fireEvent.click(within(panel).getByRole('button', { name: '扫描会话存储' }));

    await waitFor(() => expect(apiMocks.scanSessionStorage).toHaveBeenCalledTimes(1));
    expect(await within(panel).findByText('检测到旧版存储，建议迁移', { exact: false })).toBeTruthy();
    expect(within(panel).queryByText('自动删除', { exact: false })).toBeNull();
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
    apiMocks.installUpdate.mockRejectedValue({
      message: 'digest mismatch',
      operationId: 'install-update-1780000000000-42-1',
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const install = await screen.findByRole('button', { name: '立即更新' });
    fireEvent.click(install);
    expect(await screen.findByText('digest mismatch')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '导出本次诊断' }));
    await waitFor(() => expect(apiMocks.exportDiagnostics)
      .toHaveBeenCalledWith('install-update-1780000000000-42-1'));
    await waitFor(() => expect((screen.getByRole('button', { name: '立即更新' }) as HTMLButtonElement).disabled).toBe(false));
  });

  it('starts only one pending update flow on a same-tick double click', async () => {
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: null, checkedAtMs: 10,
    });
    const pending = deferred<UpdateInstallReceipt>();
    apiMocks.installUpdate.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const install = await screen.findByRole('button', { name: '立即更新' });
    act(() => {
      install.click();
      install.click();
    });

    expect(apiMocks.installUpdate).toHaveBeenCalledTimes(1);
    expect(screen.getAllByRole('region', { name: '发现新版本' })).toHaveLength(1);
    expect((screen.getByRole('button', { name: '正在下载并安装…' }) as HTMLButtonElement).disabled)
      .toBe(true);

    pending.reject(new Error('injected update failure'));
    await screen.findByText('injected update failure');
  });

  it('serializes update installation against a retained mutation handler in the same tick', async () => {
    apiMocks.checkForUpdates.mockResolvedValue({
      currentVersion: '0.1.5', latestVersion: '0.1.6', updateAvailable: true,
      releaseNotes: null, checkedAtMs: 10,
    });
    const pending = deferred<UpdateInstallReceipt>();
    apiMocks.installUpdate.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const install = await screen.findByRole('button', { name: '立即更新' });
    const switchButton = screen.getByRole('button', { name: '切换到 ChatGPT 账号' });
    const retainedSwitch = retainReactClickHandler(switchButton);
    act(() => {
      install.click();
      retainedSwitch();
    });

    expect(apiMocks.installUpdate).toHaveBeenCalledTimes(1);
    expect(apiMocks.switchRuntime).not.toHaveBeenCalled();
    expect(screen.queryByRole('region', { name: '运行态切换进度' })).toBeNull();

    pending.reject(new Error('injected update failure'));
    await screen.findByText('injected update failure');
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
    const sync = screen.getByRole('button', { name: '会话合并与修复' }) as HTMLButtonElement;
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

  it('renders saved and current without presenting stale verification health', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const account = await screen.findByRole('article', { name: 'ChatGPT 账号态' });
    const relay = screen.getByRole('article', { name: 'API 中转站态' });
    expect(within(account).getByText('已保存')).toBeTruthy();
    expect(within(account).getByText('非当前')).toBeTruthy();
    expect(within(account).queryByText('未验证')).toBeNull();
    expect(within(relay).getByText('当前运行')).toBeTruthy();
    expect(within(relay).queryByText('已验证')).toBeNull();
    expect((within(relay).getByRole('button', { name: '当前为中转站' }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getAllByText(/任务执行器会安全关闭，并在成功后自动打开 ChatGPT/)).toHaveLength(2);
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

  it('opens diagnostics from the toolbar as a page region and restores trigger focus', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    const trigger = await screen.findByRole('button', { name: '诊断' });
    trigger.focus();
    fireEvent.click(trigger);

    const panel = screen.getByRole('region', { name: '诊断与支持' });
    expect(trigger.getAttribute('aria-expanded')).toBe('true');
    expect(trigger.getAttribute('aria-controls')).toBe('diagnostic-panel');
    expect(panel.textContent).toContain('已自动脱敏，不含凭据和聊天内容');
    await waitFor(() => expect(document.activeElement).toBe(
      within(panel).getByRole('heading', { name: '诊断与支持' }),
    ));

    fireEvent.click(within(panel).getByRole('button', { name: '关闭诊断面板' }));
    await waitFor(() => expect(screen.queryByRole('region', { name: '诊断与支持' })).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  it('keeps the diagnostic panel mounted while export is busy and restores focus after completion', async () => {
    let resolveExport: ((receipt: object) => void) | undefined;
    apiMocks.exportDiagnostics.mockImplementationOnce(
      () => new Promise((resolve) => { resolveExport = resolve; }),
    );
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    const trigger = await screen.findByRole('button', { name: '诊断' });
    trigger.focus();
    fireEvent.click(trigger);
    const panel = screen.getByRole('region', { name: '诊断与支持' });
    const exportButton = within(panel).getByRole('button', { name: '导出最近诊断' });
    exportButton.focus();
    fireEvent.click(exportButton);

    await waitFor(() => expect(trigger).toHaveProperty('disabled', true));
    fireEvent.click(trigger);
    expect(screen.getByRole('region', { name: '诊断与支持' })).toBeTruthy();
    expect(apiMocks.exportDiagnostics).toHaveBeenCalledTimes(1);

    await act(async () => {
      resolveExport?.({
        exportId: 'export-busy-1',
        path: 'C:\\isolated\\diagnostics.zip',
        filename: 'diagnostics.zip',
        bytes: 128,
        sha256: 'a'.repeat(64),
        eventCount: 1,
        selection: {
          mode: 'retainedWindow',
          fromTimestampMs: 1,
          throughTimestampMs: 2,
        },
        warnings: [],
      });
    });
    await screen.findByText('诊断包已保存');
    await waitFor(() => expect(trigger).toHaveProperty('disabled', false));
    expect(document.activeElement).toBe(exportButton);

    trigger.focus();
    fireEvent.click(trigger);
    await waitFor(() => expect(screen.queryByRole('region', { name: '诊断与支持' })).toBeNull());
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  it('records only fixed safe classifications for unhandled frontend failures', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    await screen.findByRole('button', { name: '诊断' });

    window.dispatchEvent(new ErrorEvent('error', {
      message: 'sk-user-secret',
      filename: 'file:///C:/Users/alice/private.ts',
      error: new Error('private stack value'),
    }));

    await waitFor(() => expect(apiMocks.recordFrontendDiagnostic).toHaveBeenCalledTimes(1));
    expect(apiMocks.recordFrontendDiagnostic).toHaveBeenCalledWith({
      level: 'error',
      component: 'frontend',
      eventKind: 'unhandledError',
      errorCode: 'frontend.unhandled_error',
      safeMessage: '前端发生未处理异常',
    });
    expect(JSON.stringify(apiMocks.recordFrontendDiagnostic.mock.calls)).not.toContain('sk-user-secret');
    expect(JSON.stringify(apiMocks.recordFrontendDiagnostic.mock.calls)).not.toContain('alice');
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
      sessionStorage: DashboardData['sessionStorage'];
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
      sessionStorage: DashboardData['sessionStorage'];
      runtimes: DashboardData['runtimes'];
      runtimeStatus: DashboardData['runtimeStatus'];
      operations: DashboardData['operations'];
    }>();
    apiMocks.upsertRelayRuntime.mockRejectedValue({
      message: 'relay store unavailable',
      operationId: 'save-relay-1780000000000-42-2',
    });
    apiMocks.loadRuntimeDashboard.mockReturnValueOnce(pendingRefresh.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));
    const panel = screen.getByRole('region', { name: '配置 API 中转站' });
    const key = within(panel).getByLabelText('API Key') as HTMLInputElement;
    fireEvent.change(key, { target: { value: 'sk-retry-value' } });
    fireEvent.click(within(panel).getByRole('button', { name: '保存中转站' }));

    expect(await within(panel).findByText('relay store unavailable')).toBeTruthy();
    expect(screen.getAllByText('relay store unavailable')).toHaveLength(1);
    fireEvent.click(within(panel).getByRole('button', { name: '导出本次诊断' }));
    await waitFor(() => expect(apiMocks.exportDiagnostics)
      .toHaveBeenCalledWith('save-relay-1780000000000-42-2'));
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
    expect((screen.getByRole('button', { name: '会话合并与修复' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '保存当前账号态' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('keeps managed-session mutations available when only the independent sync scan fails', async () => {
    const data = dashboardData();
    data.sessions = { status: 'error', error: 'independent inventory failed' };
    if (data.managedSessions.status !== 'ready') throw new Error('fixture mismatch');
    data.managedSessions.data.sessions = [{
      id: 'thread-a', title: '可管理会话', preview: null, modelProvider: 'openai',
      updatedAt: 1, updatedAtMs: 1000, archived: true, archivedAt: 1000, scope: 'current',
      current: { home: 'C:\\Users\\alice\\.codex', rolloutPath: 'sessions/thread-a.jsonl', sessionFile: 'sessions/thread-a.jsonl', archived: true, archivedAt: 1000, updatedAt: 1, updatedAtMs: 1000 },
      shared: null,
    }];
    data.managedSessions.data.totalCount = 1;
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话' }));
    fireEvent.click(screen.getByLabelText(/^选择 thread-a/));
    expect((screen.getByRole('button', { name: '会话合并与修复' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '恢复可见' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('gates account import on its required files while trusting successful session domains', async () => {
    const data = dashboardData();
    if (data.codexHome.status !== 'ready') throw new Error('fixture must include Codex Home');
    data.codexHome.data.authJson.exists = false;
    data.codexHome.data.stateDb.exists = false;
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    expect((await screen.findByRole('button', { name: '保存当前账号态' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '会话合并与修复' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('does not offer Account save or switch for a non-official auth file', async () => {
    const data = dashboardData();
    if (data.codexHome.status !== 'ready') throw new Error('fixture must include Codex Home');
    if (data.runtimeStatus.status !== 'ready') throw new Error('fixture must include runtime status');
    data.codexHome.data.authSummary = {
      authMode: 'apikey',
      topLevelKeys: ['auth_mode'],
      hasTokensObject: false,
    };
    data.runtimeStatus.data.authMode = 'apikey';
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    expect((await screen.findByRole('button', { name: '保存当前账号态' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '切换到 ChatGPT 账号' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '配置中转站' }) as HTMLButtonElement).disabled).toBe(false);
  });

  it('keeps merge and repair disabled until the v0.3 storage migration is complete', async () => {
    const data = dashboardData();
    data.sessionStorage = { status: 'ready', data: shadowScanReport() };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    expect((await screen.findByRole('button', { name: '会话合并与修复' }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText('完成 v0.3 前台迁移后可用；旧完全同步已停用。')).toBeTruthy();
  });

  it('keeps Relay configurable and switchable when Account auth and config are missing', async () => {
    const data = dashboardData();
    if (data.codexHome.status !== 'ready') throw new Error('fixture mismatch');
    if (data.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    data.codexHome.data.authJson.exists = false;
    data.codexHome.data.configToml.exists = false;
    data.codexHome.data.authSummary = null;
    data.runtimeStatus.data = {
      activeRuntimeId: null,
      confidence: 'unknown',
      authMode: null,
      modelProvider: null,
      detectedAtMs: 6,
    };

    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const notice = await screen.findByRole('region', { name: 'Account 尚未就绪' });
    expect(within(notice).getByRole('heading', {
      name: 'Account 需要官方登录；中转站仍可直接使用',
    })).toBeTruthy();
    expect(within(notice).getByText(/不会创建、覆盖 auth\.json/)).toBeTruthy();
    const safety = screen.getByRole('complementary', { name: '安全检查' });
    expect(within(safety).getByText('ChatGPT 数据文件：缺失')).toBeTruthy();
    expect((screen.getByRole('button', { name: '保存当前账号态' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '切换到 ChatGPT 账号' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '配置中转站' }) as HTMLButtonElement).disabled).toBe(false);
    expect((screen.getByRole('button', { name: '切换到中转站' }) as HTMLButtonElement).disabled).toBe(false);
    expect(screen.getByText('切换到 Account 前需要先在 ChatGPT 完成官方登录。')).toBeTruthy();
  });

  it('does not let an immediate mutation race startup continuity initialization', async () => {
    const continuity = deferred<Awaited<ReturnType<typeof import('./api').getMobileContinuityStatus>>>();
    apiMocks.getMobileContinuityStatus.mockReturnValue(continuity.promise);
    const data = dashboardData();
    if (data.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    data.runtimeStatus.data = {
      activeRuntimeId: 'plus',
      confidence: 'exact',
      authMode: 'chatgpt',
      modelProvider: 'openai',
      detectedAtMs: 5,
    };

    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const configure = await screen.findByRole('button', { name: '配置中转站' });
    const switchRelay = screen.getByRole('button', { name: '切换到中转站' });
    expect((configure as HTMLButtonElement).disabled).toBe(true);
    expect((switchRelay as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText('连续性状态读取中…')).toBeTruthy();

    continuity.resolve({
      enabled: true,
      noticePending: false,
      initializedAtMs: 1,
      queued: 0,
      publishing: 0,
      remotePublished: 0,
      partial: 0,
      conflict: 0,
      needsManual: 0,
      items: [],
    });

    await waitFor(() => expect((configure as HTMLButtonElement).disabled).toBe(false));
    expect((switchRelay as HTMLButtonElement).disabled).toBe(false);
  });

  it('keeps relay configuration and backup recovery available when Codex Home itself is damaged', async () => {
    const data = dashboardData();
    data.codexHome = { status: 'error', error: 'auth.json is malformed' };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const relay = await screen.findByRole('article', { name: 'API 中转站态' });
    expect((within(relay).getByRole('button', { name: '配置中转站' }) as HTMLButtonElement).disabled).toBe(false);
    expect(within(relay).queryByRole('button', { name: '验证连接' })).toBeNull();
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

  it('confirms one merge and repair operation without provider copies and renders the backend receipt', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    const receipt = {
      operationId: 'sync-1', backups: [{ backupDir: 'C:\\backups\\sync-1' }],
      insertedThreads: 3, copiedSessionFiles: 2, duplicateThreads: 8,
      skippedMissingSessionFiles: 1, skippedArchivedThreads: 0, mergedSessionIndexEntries: 2,
      persistentSessionBytesAdded: 2048, persistentSessionBytesReclaimed: 1024,
      warnings: ['审计日志写入失败'],
      chatgptLaunch: { status: 'launched', message: null },
    };
    apiMocks.mergeAndRepairSessions.mockImplementation(async (onProgress) => {
      onProgress({ phase: 'preparing', timestampMs: 100 });
      onProgress({ phase: 'closingApp', timestampMs: 300 });
      onProgress({ phase: 'backingUp', timestampMs: 800 });
      onProgress({ phase: 'reconciling', timestampMs: 1_000 });
      onProgress({ phase: 'recordingResult', timestampMs: 2_500 });
      onProgress({ phase: 'launchingApp', timestampMs: 2_600 });
      onProgress({ phase: 'complete', timestampMs: 2_800 });
      return receipt;
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);
    fireEvent.click(await screen.findByRole('button', { name: '会话合并与修复' }));

    const confirmation = await screen.findByRole('region', { name: '会话合并与修复' });
    expect(within(confirmation).getByText('不生成 provider 正文副本')).toBeTruthy();
    expect(within(confirmation).getByText('冲突默认不覆盖')).toBeTruthy();
    fireEvent.click(within(confirmation).getByRole('button', { name: '开始合并与修复' }));
    await waitFor(() => expect(apiMocks.mergeAndRepairSessions).toHaveBeenCalled());
    expect(await screen.findByText('操作 ID：sync-1')).toBeTruthy();
    expect(screen.getByText('新增线程：3')).toBeTruthy();
    expect(screen.getByText('耗时·对账活跃会话：1.5s')).toBeTruthy();
    expect(screen.getByText('合并与修复总耗时：2.7s')).toBeTruthy();
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
    apiMocks.mergeAndRepairSessions.mockResolvedValue({
      operationId: 'sync-refresh-failed', backups: [], insertedThreads: 1,
      copiedSessionFiles: 1, duplicateThreads: 0, skippedMissingSessionFiles: 0,
      skippedArchivedThreads: 0, mergedSessionIndexEntries: 1,
      persistentSessionBytesAdded: 0, persistentSessionBytesReclaimed: 0,
      chatgptLaunch: { status: 'launched', message: null },
    });
    render(<App loadDashboard={load} />);
    fireEvent.click(await screen.findByRole('button', { name: '会话合并与修复' }));
    fireEvent.click(await screen.findByRole('button', { name: '开始合并与修复' }));

    expect(await screen.findByText('操作 ID：sync-refresh-failed')).toBeTruthy();
    expect(screen.getByText('会话净变化：0 B')).toBeTruthy();
    expect(screen.queryByText('会话净变化：+0 B')).toBeNull();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    const syncEnabledBeforeRefresh = !(screen.getByRole('button', { name: '会话合并与修复' }) as HTMLButtonElement).disabled;
    pendingRefresh.reject(new Error('refresh failed'));
    expect(await screen.findByText(/操作已成功，但状态刷新失败：refresh failed/)).toBeTruthy();
    expect(syncEnabledBeforeRefresh).toBe(true);
  });

  it('refreshes durable history after a failed session sync', async () => {
    const pendingRefresh = deferred<DashboardData>();
    const failed = dashboardData();
    if (failed.operations.status !== 'ready') throw new Error('fixture mismatch');
    failed.operations.data = [{
      operationId: 'sync-failed-1', action: 'syncSessions', status: 'failed', phase: 'apply',
      startedAtMs: 20, completedAtMs: 21, backupDirs: ['C:\\backups\\sync-failed'], counts: {},
    }];
    const load = vi.fn()
      .mockResolvedValueOnce(dashboardData())
      .mockReturnValueOnce(pendingRefresh.promise);
    apiMocks.mergeAndRepairSessions.mockImplementation(async (onProgress) => {
      onProgress({
        phase: 'failed',
        timestampMs: 21,
        operationId: 'sync-failed-1',
      });
      throw new Error('sync apply failed');
    });
    render(<App loadDashboard={load} />);
    fireEvent.click(await screen.findByRole('button', { name: '会话合并与修复' }));
    fireEvent.click(await screen.findByRole('button', { name: '开始合并与修复' }));

    expect(await screen.findByText('sync apply failed')).toBeTruthy();
    expect(screen.getByRole('button', { name: '导出本次诊断' })).toBeTruthy();
    await waitFor(() => expect(load).toHaveBeenCalledTimes(2));
    const syncEnabledBeforeRefresh = !(screen.getByRole('button', { name: '会话合并与修复' }) as HTMLButtonElement).disabled;
    pendingRefresh.resolve(failed);
    const history = await screen.findByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('sync-failed-1')).toBeTruthy();
    expect(within(history).getByText('失败')).toBeTruthy();
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
    expect(screen.getByText('已验证完整备份会持续保留，可逐份恢复或删除')).toBeTruthy();
    expect(screen.getByText('来源：C:\\shared-sessions')).toBeTruthy();
    expect(screen.getAllByRole('button', { name: /^恢复此备份/ })).toHaveLength(2);
    expect(screen.getAllByRole('button', { name: /^删除恢复点/ })).toHaveLength(2);
    expect(screen.getByText(/请求端切换不创建检查点/)).toBeTruthy();
    expect(screen.getByText(/会话同步等写操作只创建覆盖实际写集的轻量临时点/)).toBeTruthy();
  });

  it('keeps verified full backups beyond the first five browsable and manageable', async () => {
    const data = dashboardData();
    if (data.backups.status !== 'ready') throw new Error('fixture mismatch');
    for (let index = 2; index <= 7; index += 1) {
      data.backups.data.push({
        backupDir: `C:\\backups\\safe-${index}`,
        sourceRoot: index % 2 === 0 ? 'C:\\shared-sessions' : 'C:\\Users\\alice\\.codex',
        reason: 'manual-full-backup',
        createdAtMs: 10 - index,
        fileCount: 8,
        totalBytes: 8192,
        verified: true,
        completeSessions: true,
      });
    }
    data.backups.data.push({
      backupDir: 'C:\\backups\\unverified',
      sourceRoot: 'C:\\Users\\alice\\.codex',
      reason: 'manual-full-backup',
      createdAtMs: 1,
      fileCount: 8,
      totalBytes: 8192,
      verified: false,
      completeSessions: true,
    });
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const list = await screen.findByRole('region', { name: '已验证完整备份，共 7 份' });
    expect(list.className).toContain('backup-list');
    expect(within(list).getAllByRole('button', { name: /^恢复此备份/ })).toHaveLength(7);
    expect(within(list).getAllByRole('button', { name: /^删除恢复点/ })).toHaveLength(7);
    fireEvent.click(within(list).getByRole('button', { name: /删除恢复点.*safe-7/ }));
    const confirmation = screen.getByRole('region', { name: '删除此恢复点' });
    expect(within(confirmation).getByText('路径：C:\\backups\\safe-7')).toBeTruthy();
    fireEvent.click(within(confirmation).getByRole('button', { name: '取消' }));
    expect(apiMocks.deleteBackup).not.toHaveBeenCalled();
    expect(within(list).queryByText('C:\\backups\\unverified')).toBeNull();
  });

  it('cancels backup deletion inline without invoking the backend', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const deleteButton = await screen.findByRole('button', { name: /^删除恢复点/ });
    fireEvent.click(deleteButton);

    const confirmation = screen.getByRole('region', { name: '删除此恢复点' });
    expect(within(confirmation).getByText(
      '这会永久删除备份本身；删除后无法从 ChatGPT Switch 恢复。当前 ChatGPT 数据不会被删除。',
    )).toBeTruthy();
    expect(within(confirmation).getByText('路径：C:\\backups\\safe-1')).toBeTruthy();
    expect(apiMocks.deleteBackup).not.toHaveBeenCalled();

    fireEvent.click(within(confirmation).getByRole('button', { name: '取消' }));

    expect(screen.queryByRole('region', { name: '删除此恢复点' })).toBeNull();
    expect(apiMocks.deleteBackup).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: /^删除恢复点/ })).toBeTruthy();
  });

  it('deletes a verified backup once, disables backup actions while busy, and refreshes the list', async () => {
    const pending = deferred<BackupDeleteReceipt>();
    apiMocks.deleteBackup.mockReturnValue(pending.promise);
    const refreshed = dashboardData();
    if (
      refreshed.backups.status !== 'ready'
      || refreshed.operations.status !== 'ready'
      || refreshed.backupStorage.status !== 'ready'
    ) throw new Error('fixture mismatch');
    refreshed.backups.data = [];
    refreshed.operations.data = [{
      operationId: 'delete-backup-1',
      action: 'deleteBackup',
      status: 'succeeded',
      phase: 'complete',
      startedAtMs: 20,
      completedAtMs: 21,
      backupDirs: ['C:\\backups\\safe-1'],
      counts: { reclaimedBytes: 4096 },
    }];
    apiMocks.loadBackupDashboard.mockResolvedValueOnce({
      backups: refreshed.backups,
      backupStorage: refreshed.backupStorage,
      operations: refreshed.operations,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: /^删除恢复点/ }));
    const confirmation = screen.getByRole('region', { name: '删除此恢复点' });
    const confirmDelete = within(confirmation).getByRole('button', { name: '确认删除恢复点' });
    act(() => {
      confirmDelete.click();
      confirmDelete.click();
    });

    expect(apiMocks.deleteBackup).toHaveBeenCalledTimes(1);
    expect(apiMocks.deleteBackup).toHaveBeenCalledWith('C:\\backups\\safe-1', true);
    expect(screen.getByText('删除恢复点处理中')).toBeTruthy();
    expect((screen.getByRole('button', { name: /^恢复此备份/ }) as HTMLButtonElement).disabled)
      .toBe(true);
    expect((screen.getByRole('button', { name: /^删除恢复点/ }) as HTMLButtonElement).disabled)
      .toBe(true);

    pending.resolve({
      operationId: 'delete-backup-1',
      backupDir: 'C:\\backups\\safe-1',
      reclaimedBytes: 4096,
      warnings: [],
    });

    expect(await screen.findByText('恢复点已删除')).toBeTruthy();
    expect(screen.getByText('操作 ID：delete-backup-1')).toBeTruthy();
    expect(screen.getByText('已回收：4.0 KiB')).toBeTruthy();
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1));
    await waitFor(() => expect(screen.queryByRole('button', { name: /^删除恢复点/ })).toBeNull());
    const history = screen.getByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('删除恢复点')).toBeTruthy();
  });

  it('keeps a backup available when deletion fails', async () => {
    apiMocks.deleteBackup.mockRejectedValue(new Error('backup changed during verification'));
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: /^删除恢复点/ }));
    fireEvent.click(screen.getByRole('button', { name: '确认删除恢复点' }));

    expect(await screen.findByText('backup changed during verification')).toBeTruthy();
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1));
    const backupPanel = screen.getByRole('complementary', { name: '备份与恢复' });
    expect(within(backupPanel).getByRole('button', { name: /^删除恢复点/ })).toBeTruthy();
    expect(within(backupPanel).getByText('C:\\backups\\safe-1')).toBeTruthy();
  });

  it('creates a full backup with immediate loading, typed receipt, and one background refresh', async () => {
    const pending = deferred<CreateFullBackupReceipt>();
    apiMocks.createFullBackup.mockReturnValue(pending.promise);
    const refreshed = dashboardData();
    if (refreshed.operations.status !== 'ready') throw new Error('fixture mismatch');
    refreshed.operations.data = [{
      operationId: 'backup-manual-1',
      action: 'createBackup',
      status: 'succeeded',
      phase: 'complete',
      startedAtMs: 19,
      completedAtMs: 21,
      backupDirs: [
        'C:\\backups\\manual-current-1',
        'C:\\backups\\manual-shared-1',
      ],
      counts: { backupFiles: 8 },
    }];
    apiMocks.loadBackupDashboard.mockResolvedValueOnce({
      backups: refreshed.backups,
      operations: refreshed.operations,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const create = await screen.findByRole('button', { name: '创建完整备份' });
    act(() => {
      create.click();
      create.click();
    });

    expect(apiMocks.createFullBackup).toHaveBeenCalledTimes(1);
    expect(screen.getByRole('button', { name: '正在创建完整备份' })).toBeTruthy();
    expect(screen.getByText('创建完整备份处理中')).toBeTruthy();
    pending.resolve({
      operationId: 'backup-manual-1',
      backups: [
        {
          backupDir: 'C:\\backups\\manual-current-1',
          sourceRoot: 'C:\\Users\\alice\\.codex',
          reason: 'manual-full-backup',
          createdAtMs: 20,
          scope: 'full',
          trackedDatabaseCount: 4,
          completeSessions: true,
        },
        {
          backupDir: 'C:\\backups\\manual-shared-1',
          sourceRoot: 'C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\shared-sessions',
          reason: 'manual-full-backup',
          createdAtMs: 21,
          scope: 'full',
          trackedDatabaseCount: 4,
          completeSessions: true,
        },
      ],
      warnings: ['ChatGPT 已关闭'],
    });

    expect(await screen.findByText('完整备份已创建')).toBeTruthy();
    expect(screen.getByText('操作 ID：backup-manual-1')).toBeTruthy();
    expect(screen.getByText('备份：2')).toBeTruthy();
    expect(screen.getByText('来源 1：C:\\Users\\alice\\.codex · 受管数据库：4')).toBeTruthy();
    expect(screen.getByText(
      '来源 2：C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\shared-sessions · 受管数据库：4',
    )).toBeTruthy();
    expect(screen.getByText('备份路径：C:\\backups\\manual-current-1')).toBeTruthy();
    expect(screen.getByText('备份路径：C:\\backups\\manual-shared-1')).toBeTruthy();
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1));
    const history = screen.getByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('backup-manual-1')).toBeTruthy();
    expect(within(history).getByText('创建完整备份')).toBeTruthy();
  });

  it('shows an honest indeterminate inline task while reclaiming proven automatic checkpoints', async () => {
    const pending = deferred<CheckpointCleanupReceipt>();
    apiMocks.cleanupAutomaticCheckpoints.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    const cleanup = await screen.findByRole('button', { name: '安全释放 1.4 GiB' });
    act(() => {
      cleanup.click();
      cleanup.click();
    });

    expect(apiMocks.cleanupAutomaticCheckpoints).toHaveBeenCalledTimes(1);
    const flow = screen.getByRole('region', { name: '自动检查点清理进度' });
    expect(within(flow).getByText('正在执行安全清理任务')).toBeTruthy();
    expect(within(flow).getByRole('status').textContent).toContain('正在核对持久化终态');
    expect(within(flow).queryByRole('list')).toBeNull();
    expect(screen.queryByRole('button', { name: '正在安全释放' })).toBeNull();
    expect(screen.getByText('清理自动检查点处理中')).toBeTruthy();

    pending.resolve({
      operationId: 'cleanup-1',
      attemptedCount: 2,
      failedCount: 0,
      reclaimedCount: 2,
      reclaimedBytes: 1_471_410_293,
      retainedCount: 17,
      warnings: [],
    });

    expect(await within(flow).findByText('安全清理任务已完成')).toBeTruthy();
    expect(within(flow).getByText(/已释放 1.4 GiB/)).toBeTruthy();
    expect(await screen.findByText('操作 ID：cleanup-1')).toBeTruthy();
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1));
  });

  it('keeps checkpoint cleanup failures inline without opening a native dialog', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    const alert = vi.spyOn(window, 'alert');
    const prompt = vi.spyOn(window, 'prompt');
    apiMocks.cleanupAutomaticCheckpoints.mockRejectedValue(new Error('operation log is damaged'));
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '安全释放 1.4 GiB' }));

    const flow = await screen.findByRole('region', { name: '自动检查点清理进度' });
    expect(await within(flow).findByText('安全清理任务未完成')).toBeTruthy();
    expect(within(flow).getByRole('alert').textContent).toContain('operation log is damaged');
    expect(confirm).not.toHaveBeenCalled();
    expect(alert).not.toHaveBeenCalled();
    expect(prompt).not.toHaveBeenCalled();
    expect(document.querySelector('dialog')).toBeNull();
  });

  it('treats retained informational warnings as success when every planned cleanup succeeds', async () => {
    apiMocks.cleanupAutomaticCheckpoints.mockResolvedValue({
      operationId: 'cleanup-with-notes',
      attemptedCount: 4,
      failedCount: 0,
      reclaimedCount: 4,
      reclaimedBytes: 3_633_111_652,
      retainedCount: 18,
      warnings: ['未分类目录继续安全保留'],
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '安全释放 1.4 GiB' }));

    const flow = await screen.findByRole('region', { name: '自动检查点清理进度' });
    expect(await within(flow).findByText('安全清理任务已完成（有保留说明）')).toBeTruthy();
    expect(screen.getByText('自动检查点清理完成（有保留说明）')).toBeTruthy();
    expect(screen.getByText('计划：4')).toBeTruthy();
    expect(screen.getByText('失败：0')).toBeTruthy();
    expect(screen.getByText('安全保留：18')).toBeTruthy();
    expect(screen.getByText('警告：未分类目录继续安全保留')).toBeTruthy();
    expect(screen.queryByText('自动检查点部分完成')).toBeNull();
  });

  it('labels real checkpoint deletion failures as partial even without warning text', async () => {
    apiMocks.cleanupAutomaticCheckpoints.mockResolvedValue({
      operationId: 'cleanup-partial',
      attemptedCount: 2,
      failedCount: 1,
      reclaimedCount: 1,
      reclaimedBytes: 1024,
      retainedCount: 18,
      warnings: [],
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '安全释放 1.4 GiB' }));

    const flow = await screen.findByRole('region', { name: '自动检查点清理进度' });
    expect(await within(flow).findByText('安全清理任务部分完成')).toBeTruthy();
    expect(screen.getByText('自动检查点部分完成')).toBeTruthy();
    expect(screen.getByText('计划：2')).toBeTruthy();
    expect(screen.getByText('失败：1')).toBeTruthy();
    expect(screen.queryByText('自动检查点清理完成（有保留说明）')).toBeNull();
  });

  it('queues a fresh backup scan when another mutation finishes during an older scan', async () => {
    const firstRefresh = deferred<BackupDashboardData>();
    const secondRefresh = deferred<BackupDashboardData>();
    apiMocks.mergeAndRepairSessions.mockResolvedValue({
      operationId: 'sync-after-cleanup',
      backups: [],
      insertedThreads: 0,
      copiedSessionFiles: 0,
      duplicateThreads: 0,
      skippedMissingSessionFiles: 0,
      skippedArchivedThreads: 0,
      mergedSessionIndexEntries: 0,
      persistentSessionBytesAdded: 0,
      persistentSessionBytesReclaimed: 0,
      warnings: [],
      chatgptLaunch: { status: 'launched', message: null },
    });
    render(<App />);

    await screen.findByRole('article', { name: 'ChatGPT 账号态' });
    fireEvent.click(screen.getByRole('button', { name: '会话' }));
    await screen.findByRole('heading', { name: '会话管理' });
    await waitFor(() => expect(apiMocks.loadSessionDashboard).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole('button', { name: '运行态' }));
    fireEvent.click(await screen.findByRole('button', { name: '加载备份' }));
    await screen.findByRole('button', { name: '安全释放 1.4 GiB' });
    expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1);

    apiMocks.loadBackupDashboard
      .mockReturnValueOnce(firstRefresh.promise)
      .mockReturnValueOnce(secondRefresh.promise);
    fireEvent.click(screen.getByRole('button', { name: '安全释放 1.4 GiB' }));
    await screen.findByText('操作 ID：cleanup-default');
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(2));

    fireEvent.click(screen.getByRole('button', { name: '会话合并与修复' }));
    fireEvent.click(await screen.findByRole('button', { name: '开始合并与修复' }));
    await screen.findByText('操作 ID：sync-after-cleanup');

    const stale = dashboardData();
    firstRefresh.resolve({
      backups: stale.backups,
      backupStorage: stale.backupStorage,
      operations: stale.operations,
    });
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(3));

    const fresh = dashboardData();
    if (fresh.backupStorage.status !== 'ready') throw new Error('fixture mismatch');
    fresh.backupStorage.data = {
      ...fresh.backupStorage.data,
      totalCount: 17,
      totalBytes: 2_693_977_957,
      reclaimableCount: 0,
      reclaimableBytes: 0,
    };
    secondRefresh.resolve({
      backups: fresh.backups,
      backupStorage: fresh.backupStorage,
      operations: fresh.operations,
    });

    expect(await screen.findByRole('button', {
      name: '没有可安全释放的检查点',
    })).toBeTruthy();
  });

  it('refreshes durable backup history after full backup creation fails', async () => {
    const refreshed = dashboardData();
    if (refreshed.backups.status !== 'ready' || refreshed.operations.status !== 'ready') {
      throw new Error('fixture mismatch');
    }
    refreshed.backups.data = [{
      backupDir: 'C:\\backups\\manual-current-partial',
      sourceRoot: 'C:\\Users\\alice\\.codex',
      reason: 'manual-full-current',
      createdAtMs: 20,
      fileCount: 4,
      totalBytes: 4096,
      verified: true,
      completeSessions: true,
    }];
    refreshed.operations.data = [{
      operationId: 'backup-manual-failed',
      action: 'createBackup',
      status: 'failed',
      phase: 'apply',
      startedAtMs: 19,
      completedAtMs: 21,
      backupDirs: ['C:\\backups\\manual-current-partial'],
      counts: {},
    }];
    apiMocks.createFullBackup.mockRejectedValue(new Error('shared backup failed'));
    apiMocks.loadBackupDashboard.mockResolvedValueOnce({
      backups: refreshed.backups,
      operations: refreshed.operations,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '创建完整备份' }));

    expect(await screen.findByText('shared backup failed')).toBeTruthy();
    await waitFor(() => expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1));
    const history = screen.getByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('backup-manual-failed')).toBeTruthy();
    expect(within(history).getByText('C:\\backups\\manual-current-partial')).toBeTruthy();
    expect(within(history).getByText('失败')).toBeTruthy();
  });

  it('serializes same-tick backup retries with a synchronous in-flight guard', async () => {
    const data = dashboardData();
    data.backups = { status: 'error', error: 'backup index unavailable' };
    const pending = deferred<BackupDashboardData>();
    apiMocks.loadBackupDashboard.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const retry = await screen.findByRole('button', { name: '重试' });
    act(() => {
      retry.click();
      retry.click();
    });

    expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1);
    expect(screen.getByText('备份列表扫描中...')).toBeTruthy();
    const refreshed = dashboardData();
    pending.resolve({
      backups: refreshed.backups,
      backupStorage: refreshed.backupStorage,
      operations: refreshed.operations,
    });
    await waitFor(() => expect(screen.getByText('已验证完整备份会持续保留，可逐份恢复或删除')).toBeTruthy());
  });

  it('does not start a full backup while a backup scan is in flight in the same tick', async () => {
    const data = dashboardData();
    data.backups = { status: 'error', error: 'backup index unavailable' };
    const pending = deferred<BackupDashboardData>();
    apiMocks.loadBackupDashboard.mockReturnValue(pending.promise);
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const retry = await screen.findByRole('button', { name: '重试' });
    const create = screen.getByRole('button', { name: '创建完整备份' });
    act(() => {
      retry.click();
      create.click();
    });

    expect(apiMocks.loadBackupDashboard).toHaveBeenCalledTimes(1);
    expect(apiMocks.createFullBackup).not.toHaveBeenCalled();
    expect((screen.getByRole('button', { name: '创建完整备份' }) as HTMLButtonElement).disabled).toBe(true);

    const refreshed = dashboardData();
    pending.resolve({
      backups: refreshed.backups,
      backupStorage: refreshed.backupStorage,
      operations: refreshed.operations,
    });
    await waitFor(() => {
      expect((screen.getByRole('button', { name: '创建完整备份' }) as HTMLButtonElement).disabled)
        .toBe(false);
    });
  });

  it('does not conflate loading or failed domains with empty saved state', async () => {
    const data = dashboardData();
    data.runtimes = { status: 'error', error: 'runtime store unavailable' };
    data.backups = { status: 'error', error: 'backup index unavailable' };
    render(<App loadDashboard={() => Promise.resolve(data)} />);

    const account = await screen.findByRole('article', { name: 'ChatGPT 账号态' });
    expect(within(account).getAllByText('不可用').length).toBeGreaterThan(0);
    const backupPanel = screen.getByRole('complementary', { name: '备份与恢复' });
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

  it('refreshes durable operation history after a failed runtime switch', async () => {
    const failed = dashboardData();
    if (failed.operations.status !== 'ready') throw new Error('fixture mismatch');
    failed.operations.data = [{
      operationId: 'switch-failed-1', action: 'switchRuntime', status: 'failed', phase: 'apply',
      startedAtMs: 11, completedAtMs: 12, backupDirs: [], counts: {},
    }];
    apiMocks.loadRuntimeDashboard.mockResolvedValueOnce({
      codexHome: failed.codexHome,
      sessionStorage: failed.sessionStorage,
      runtimes: failed.runtimes,
      runtimeStatus: failed.runtimeStatus,
      operations: failed.operations,
    });
    apiMocks.switchRuntime.mockRejectedValue({
      message: 'request route unavailable',
      operationId: 'switch-failed-1',
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));
    expect(await screen.findByText('request route unavailable')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '关闭任务' }));
    const history = await screen.findByRole('complementary', { name: '操作历史' });
    expect(within(history).getByText('switch-failed-1')).toBeTruthy();
    expect(within(history).getByText('失败')).toBeTruthy();
  });

  it('shows one switch overlay immediately, streams stages, and makes the background inert', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('test fixture must include runtimes');
    const pendingSwitch = deferred<{
      operationId: string;
      changed: boolean;
      runtime: (typeof dashboard.runtimes.data)[number];
      incrementalSessionSync: ReturnType<typeof emptyIncrementalSyncResult>;
      routeProvenance: ReturnType<typeof recordedRouteProvenance>;
      relayValidation: 'notApplicable';
      chatProcessStateRepaired: boolean;
      chatgptLaunch: { status: 'launched'; message: null };
    }>();
    let onProgress!: (event: {
      phase: 'detectingApp' | 'closingApp' | 'preparingRuntime' | 'complete';
      timestampMs: number;
    }) => void;
    apiMocks.switchRuntime.mockImplementation((_runtimeId, callback) => {
      onProgress = callback;
      return pendingSwitch.promise;
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));

    const progress = screen.getByRole('dialog', { name: '正在切换到 ChatGPT 账号态' });
    expect(screen.getAllByRole('dialog')).toHaveLength(1);
    expect(document.querySelector('main')?.hasAttribute('inert')).toBe(true);
    expect(screen.queryByText('切换 ChatGPT 账号处理中')).toBeNull();
    expect(apiMocks.switchRuntime).toHaveBeenCalledWith('plus', expect.any(Function));
    expect(apiMocks.listCodexProcesses).not.toHaveBeenCalled();
    expect(apiMocks.closeCodexProcesses).not.toHaveBeenCalled();

    act(() => {
      onProgress({ phase: 'detectingApp', timestampMs: 100 });
      onProgress({ phase: 'closingApp', timestampMs: 110 });
      onProgress({ phase: 'preparingRuntime', timestampMs: 120 });
    });
    expect(progress.querySelector('.switch-timeline li.active')?.textContent).toContain('准备增量会话视图');
    expect(within(progress).getByText('安全关闭 ChatGPT', { selector: 'strong' }).closest('li')?.className).toBe('done');

    expect(screen.queryByRole('button', { name: '技能' })).toBeNull();
    expect(screen.getByRole('dialog')).toBeTruthy();

    act(() => onProgress({ phase: 'complete', timestampMs: 130 }));
    expect(progress.getAttribute('aria-busy')).toBe('true');
    pendingSwitch.resolve({
      operationId: 'switch-1', changed: true, runtime: dashboard.runtimes.data[0],
      incrementalSessionSync: emptyIncrementalSyncResult(),
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    expect((await screen.findAllByText('ChatGPT 已打开', { selector: 'strong' })).length)
      .toBeGreaterThan(0);
    expect(within(progress).getByText('switch-1')).toBeTruthy();
  });

  it('keeps both runtime switches disabled until the post-switch runtime scan finishes', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('fixture mismatch');
    const plusRuntime = dashboard.runtimes.data[0];
    const runtimeRefresh = deferred<RuntimeDashboardData>();
    apiMocks.loadRuntimeDashboard.mockReturnValue(runtimeRefresh.promise);
    apiMocks.switchRuntime.mockImplementation(async (_runtimeId, onProgress) => {
      onProgress({ phase: 'detectingApp', timestampMs: 100 });
      onProgress({ phase: 'complete', timestampMs: 110 });
      return {
        operationId: 'switch-refresh-pending',
        changed: true,
        runtime: plusRuntime,
        incrementalSessionSync: emptyIncrementalSyncResult(),
        routeProvenance: recordedRouteProvenance(),
        relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
        chatgptLaunch: { status: 'launched', message: null },
      };
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));
    await waitFor(() => expect(apiMocks.loadRuntimeDashboard).toHaveBeenCalledTimes(1));

    const complete = await screen.findByRole('button', { name: '完成' });
    expect((complete as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(complete);
    expect(screen.getByRole('dialog')).toBeTruthy();

    const refreshed = dashboardData();
    if (refreshed.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    refreshed.runtimeStatus.data = {
      activeRuntimeId: 'plus',
      confidence: 'mode',
      authMode: 'chatgpt',
      modelProvider: 'openai',
      detectedAtMs: 111,
    };
    runtimeRefresh.resolve({
      codexHome: refreshed.codexHome,
      sessionStorage: refreshed.sessionStorage,
      runtimes: refreshed.runtimes,
      runtimeStatus: refreshed.runtimeStatus,
      operations: refreshed.operations,
    });

    await waitFor(() => expect((complete as HTMLButtonElement).disabled).toBe(false));
    fireEvent.click(complete);
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect((screen.getByRole('button', { name: '重新应用 ChatGPT 账号' }) as HTMLButtonElement).disabled)
      .toBe(false);
    expect((screen.getByRole('button', { name: '切换到中转站' }) as HTMLButtonElement).disabled)
      .toBe(false);
  });

  it('keeps a successful switch authoritative when ChatGPT launch fails and retries inline', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('fixture mismatch');
    const plusRuntime = dashboard.runtimes.data[0];
    apiMocks.switchRuntime.mockImplementation(async (_runtimeId, onProgress) => {
      onProgress({ phase: 'recordingResult', timestampMs: 100 });
      onProgress({ phase: 'launchingApp', timestampMs: 110 });
      onProgress({ phase: 'complete', timestampMs: 120 });
      return {
        operationId: 'switch-launch-warning',
        changed: true,
        runtime: plusRuntime,
        incrementalSessionSync: emptyIncrementalSyncResult(),
        routeProvenance: recordedRouteProvenance(),
        relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
        chatgptLaunch: { status: 'failed' as const, message: 'activation unavailable' },
      };
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    const switchButton = await screen.findByRole('button', { name: '切换到 ChatGPT 账号' });
    fireEvent.click(switchButton);

    expect(await screen.findByRole('button', { name: '重试打开 ChatGPT' })).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('activation unavailable');
    expect(document.querySelector('.operation-result')).toBeNull();
    expect(document.querySelector('.busy-banner')).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: '重试打开 ChatGPT' }));
    await waitFor(() => expect(apiMocks.launchChatgpt).toHaveBeenCalledTimes(1));
    expect((await screen.findAllByText('ChatGPT 已在运行')).length).toBeGreaterThan(0);

    fireEvent.click(screen.getByRole('button', { name: '完成' }));
    await waitFor(() => expect(document.activeElement).toBe(switchButton));
  });

  it('serializes same-tick switch clicks without invalidating the session dashboard', async () => {
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
    expect(document.querySelector('main')?.hasAttribute('inert')).toBe(true);
    expect(screen.queryByRole('button', { name: '会话' })).toBeNull();
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();

    act(() => onProgress({ phase: 'complete', timestampMs: 120 }));
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();

    pendingSwitch.resolve({
      operationId: 'switch-serialized',
      changed: true,
      runtime: dashboard.runtimes.data[0],
      incrementalSessionSync: emptyIncrementalSyncResult(),
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    fireEvent.click(await screen.findByRole('button', { name: '完成' }));
    fireEvent.click(screen.getByRole('button', { name: '会话' }));
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();
  });

  it('invalidates the session dashboard only when post-switch incremental sync applied changes', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('fixture mismatch');
    apiMocks.switchRuntime.mockResolvedValue({
      operationId: 'switch-incremental-applied',
      changed: true,
      runtime: dashboard.runtimes.data[0],
      incrementalSessionSync: {
        ...emptyIncrementalSyncResult(),
        status: 'applied',
        detectedThreads: 1,
        syncedThreads: 1,
      },
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));
    fireEvent.click(await screen.findByRole('button', { name: '完成' }));
    fireEvent.click(screen.getByRole('button', { name: '会话' }));

    await waitFor(() => expect(apiMocks.loadSessionDashboard).toHaveBeenCalledTimes(1));
  });

  it('switches to relay directly without a verification prompt or network probe choice', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('fixture mismatch');
    if (dashboard.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    dashboard.runtimeStatus.data = {
      activeRuntimeId: 'plus',
      confidence: 'exact',
      authMode: 'chatgpt',
      modelProvider: 'openai',
      detectedAtMs: 20,
    };
    apiMocks.switchRuntime.mockResolvedValue({
      operationId: 'switch-direct',
      changed: true,
      runtime: {
        ...dashboard.runtimes.data[1],
      },
      incrementalSessionSync: emptyIncrementalSyncResult(),
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'skipped',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到中转站' }));
    await waitFor(() => expect(apiMocks.switchRuntime).toHaveBeenCalledWith(
      'relay',
      expect.any(Function),
    ));
    expect(screen.queryByRole('region', { name: '选择中转站切换方式' })).toBeNull();
  });

  it('ignores progress from an older switch after a newer switch has started', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('test fixture must include runtimes');
    dashboard.runtimes.data[1].relaySwitchPreference = 'direct';
    const first = deferred<RuntimeSwitchResult>();
    const second = deferred<RuntimeSwitchResult>();
    const progress: Array<(event: RuntimeSwitchProgress) => void> = [];
    apiMocks.switchRuntime
      .mockImplementationOnce((_runtimeId, callback) => {
        progress.push(callback);
        return first.promise;
      })
      .mockImplementationOnce((_runtimeId, callback) => {
        progress.push(callback);
        return second.promise;
      });
    const plusDashboard = dashboardData();
    if (plusDashboard.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    if (plusDashboard.runtimes.status !== 'ready') throw new Error('fixture mismatch');
    plusDashboard.runtimes.data[1].relaySwitchPreference = 'direct';
    plusDashboard.runtimeStatus.data = {
      activeRuntimeId: 'plus',
      confidence: 'exact',
      authMode: 'chatgpt',
      modelProvider: 'openai',
      detectedAtMs: 20,
    };
    apiMocks.loadRuntimeDashboard.mockResolvedValue({
      codexHome: plusDashboard.codexHome,
      sessionStorage: plusDashboard.sessionStorage,
      runtimes: plusDashboard.runtimes,
      runtimeStatus: plusDashboard.runtimeStatus,
      operations: plusDashboard.operations,
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));
    first.resolve({
      operationId: 'switch-first',
      changed: true,
      runtime: dashboard.runtimes.data[0],
      incrementalSessionSync: emptyIncrementalSyncResult(),
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    fireEvent.click(await screen.findByRole('button', { name: '完成' }));
    const relayButton = await screen.findByRole('button', { name: '切换到中转站' });
    fireEvent.click(relayButton);
    expect(await screen.findByRole('heading', { name: '正在切换到 API 中转站态' })).toBeTruthy();

    act(() => progress[0]({
      phase: 'failed',
      timestampMs: 30,
      message: 'stale first switch event',
      outcome: 'failedBeforeWrite',
    }));

    expect(screen.queryByText('stale first switch event')).toBeNull();
    expect(screen.getByRole('heading', { name: '正在切换到 API 中转站态' })).toBeTruthy();
    second.resolve({
      operationId: 'switch-second',
      changed: true,
      runtime: dashboard.runtimes.data[1],
      incrementalSessionSync: emptyIncrementalSyncResult(),
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    expect(await screen.findByText('switch-second')).toBeTruthy();
  });

  it('owns the close lifecycle and defers process exit until a running switch settles', async () => {
    const registration = deferred<() => void>();
    const pendingSwitch = deferred<RuntimeSwitchResult>();
    let closeHandler: ((event: { preventDefault: () => void }) => void) | undefined;
    const registerCloseGuard = vi.fn((handler: typeof closeHandler) => {
      closeHandler = handler;
      return registration.promise;
    });
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('fixture mismatch');
    if (dashboard.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    dashboard.runtimes.data[1].relaySwitchPreference = 'direct';
    dashboard.runtimeStatus.data = {
      activeRuntimeId: 'plus',
      confidence: 'exact',
      authMode: 'chatgpt',
      modelProvider: 'openai',
      detectedAtMs: 20,
    };
    apiMocks.switchRuntime.mockReturnValue(pendingSwitch.promise);

    render(
      <App
        loadDashboard={() => Promise.resolve(dashboard)}
        registerCloseGuard={registerCloseGuard}
      />,
    );

    const switchButton = await screen.findByRole('button', { name: '切换到中转站' });
    expect((switchButton as HTMLButtonElement).disabled).toBe(true);
    registration.resolve(() => undefined);
    await waitFor(() => expect((switchButton as HTMLButtonElement).disabled).toBe(false));

    fireEvent.click(switchButton);
    const during = { preventDefault: vi.fn() };
    act(() => closeHandler?.(during));
    expect(during.preventDefault).toHaveBeenCalledTimes(1);
    expect(apiMocks.requestAppExit).not.toHaveBeenCalled();
    expect(screen.getByText('已收到关闭请求')).toBeTruthy();

    pendingSwitch.resolve({
      operationId: 'switch-close-guard',
      changed: true,
      runtime: dashboard.runtimes.data[1],
      incrementalSessionSync: emptyIncrementalSyncResult(),
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    expect(await screen.findByText('switch-close-guard')).toBeTruthy();
    await waitFor(() => expect(apiMocks.requestAppExit).toHaveBeenCalledTimes(1), {
      timeout: 1_500,
    });

    const after = { preventDefault: vi.fn() };
    act(() => closeHandler?.(after));
    expect(after.preventDefault).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(apiMocks.requestAppExit).toHaveBeenCalledTimes(2));
  });

  it('routes an idle window close through the backend exit command', async () => {
    let closeHandler: ((event: { preventDefault: () => void }) => void) | undefined;
    render(
      <App
        loadDashboard={() => Promise.resolve(dashboardData())}
        registerCloseGuard={async (handler) => {
          closeHandler = handler;
          return () => undefined;
        }}
      />,
    );

    await screen.findByRole('button', { name: '切换到 ChatGPT 账号' });
    const event = { preventDefault: vi.fn() };
    act(() => closeHandler?.(event));

    expect(event.preventDefault).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(apiMocks.requestAppExit).toHaveBeenCalledTimes(1));
    expect(apiMocks.requestAppExit).toHaveBeenCalledWith();
  });

  it('asks before exiting while mobile continuity publication is active', async () => {
    const pendingSwitch = deferred<RuntimeSwitchResult>();
    let closeHandler: ((event: { preventDefault: () => void }) => void) | undefined;
    let onProgress!: (event: RuntimeSwitchProgress) => void;
    const dashboard = dashboardData();
    if (dashboard.runtimes.status !== 'ready') throw new Error('fixture mismatch');
    apiMocks.switchRuntime.mockImplementation((_runtimeId, callback) => {
      onProgress = callback;
      return pendingSwitch.promise;
    });
    render(
      <App
        loadDashboard={() => Promise.resolve(dashboard)}
        registerCloseGuard={async (handler) => {
          closeHandler = handler;
          return () => undefined;
        }}
      />,
    );

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));
    act(() => onProgress({ phase: 'syncingIncrementalSessions', timestampMs: 20 }));
    const closeEvent = { preventDefault: vi.fn() };
    act(() => closeHandler?.(closeEvent));

    expect(closeEvent.preventDefault).toHaveBeenCalledTimes(1);
    expect(await screen.findByRole('dialog', { name: '会话正在同步' })).toBeTruthy();
    expect(apiMocks.requestAppExit).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole('button', { name: '继续等待' }));
    expect(screen.queryByRole('dialog', { name: '会话正在同步' })).toBeNull();

    pendingSwitch.resolve({
      operationId: 'switch-mobile-close',
      changed: true,
      runtime: dashboard.runtimes.data[0],
      incrementalSessionSync: emptyIncrementalSyncResult(),
      routeProvenance: recordedRouteProvenance(),
      relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
      chatgptLaunch: { status: 'launched', message: null },
    });
    expect(await screen.findByText('switch-mobile-close')).toBeTruthy();
  });

  it('fails closed when the window close guard cannot be registered', async () => {
    const dashboard = dashboardData();
    if (dashboard.runtimeStatus.status !== 'ready') throw new Error('fixture mismatch');
    dashboard.runtimeStatus.data = {
      activeRuntimeId: 'plus',
      confidence: 'exact',
      authMode: 'chatgpt',
      modelProvider: 'openai',
      detectedAtMs: 20,
    };
    render(
      <App
        loadDashboard={() => Promise.resolve(dashboard)}
        registerCloseGuard={() => Promise.reject(new Error('listener unavailable'))}
      />,
    );

    expect(await screen.findByText(/窗口保护初始化失败/)).toBeTruthy();
    expect((screen.getByRole('button', { name: '切换到中转站' }) as HTMLButtonElement).disabled)
      .toBe(true);
    expect(screen.getAllByText(/无法确认本机 Codex 写入进程/)).toHaveLength(2);
    expect(apiMocks.switchRuntime).not.toHaveBeenCalled();
  });

  it('does not invalidate session or backup dashboards during a request-route failure', async () => {
    const pendingSwitch = deferred<RuntimeSwitchResult>();
    let onProgress!: (event: RuntimeSwitchProgress) => void;
    apiMocks.switchRuntime.mockImplementation((_runtimeId, callback) => {
      onProgress = callback;
      return pendingSwitch.promise;
    });
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到 ChatGPT 账号' }));
    act(() => onProgress({ phase: 'applyingRuntime', timestampMs: 100 }));

    act(() => {
      onProgress({
        phase: 'failed',
        timestampMs: 120,
        message: 'request config apply failed',
        outcome: 'rolledBack',
      });
      pendingSwitch.reject(new Error('request config apply failed'));
    });
    expect((await screen.findAllByText(/request config apply failed/)).length).toBeGreaterThan(0);
    fireEvent.click(screen.getByRole('button', { name: '关闭任务' }));
    expect(apiMocks.loadBackupDashboard).not.toHaveBeenCalled();
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();
  });

function emptyIncrementalSyncResult() {
  return {
    status: 'unchanged' as const,
    detectedThreads: 0,
    syncedThreads: 0,
    projectedBytes: 0,
    durationMs: 0,
    requiresFullSync: false,
  };
}

function recordedRouteProvenance() {
  return { status: 'recorded' as const };
}

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
        incrementalSessionSync: emptyIncrementalSyncResult(),
        routeProvenance: recordedRouteProvenance(),
        relayValidation: 'notApplicable',
      chatProcessStateRepaired: false,
        chatgptLaunch: { status: 'launched', message: null },
      };
    });

    render(<App />);

    await screen.findByRole('article', { name: 'ChatGPT 账号态' });
    expect(apiMocks.loadRuntimeDashboard).toHaveBeenCalledTimes(1);
    const storage = await screen.findByRole('complementary', { name: '会话存储状态' });
    expect(within(storage).getByText('在线仅扫描，不删除', { exact: false })).toBeTruthy();
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();
    expect(apiMocks.loadBackupDashboard).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: '切换到 ChatGPT 账号' }));
    await waitFor(() => expect(apiMocks.loadRuntimeDashboard).toHaveBeenCalledTimes(2));
    expect(apiMocks.loadSessionDashboard).not.toHaveBeenCalled();
    expect(apiMocks.loadBackupDashboard).not.toHaveBeenCalled();

    fireEvent.click(await screen.findByRole('button', { name: '完成' }));
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

    const progress = await screen.findByRole('dialog', { name: '切换到 ChatGPT 账号态' });
    await waitFor(() => expect(within(progress).getByText('已恢复原始请求配置')).toBeTruthy());
    const interrupted = within(progress).getByText('应用请求端配置', { selector: 'strong' }).closest('li');
    expect(interrupted?.className).toBe('failed');
    expect(within(interrupted as HTMLElement).getByText('中断')).toBeTruthy();
    expect(within(progress).queryByText('正在恢复原始请求配置')).toBeNull();
  });

  it('does not expose the retired hard-delete flow or close ChatGPT for it', async () => {
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
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话' }));
    fireEvent.click(screen.getByLabelText(/^选择 thread-a/));
    expect(screen.queryByRole('button', { name: /删除所选|确认删除|硬删除/ })).toBeNull();
    expect(screen.getByText(/v0\.3 不提供直接硬删除/)).toBeTruthy();
    expect(apiMocks.closeCodexProcesses).not.toHaveBeenCalled();
  });

  it('does not present a restore-visible checkpoint after the backend has reclaimed it', async () => {
    const dashboard = dashboardData();
    if (dashboard.managedSessions.status !== 'ready') throw new Error('fixture mismatch');
    dashboard.managedSessions.data.sessions = [{
      id: 'thread-restore', title: '待恢复', preview: null, modelProvider: 'openai',
      updatedAt: 1, updatedAtMs: 1000, archived: true, archivedAt: 1000, scope: 'current',
      current: {
        home: 'C:\\Users\\alice\\.codex', rolloutPath: 'sessions/thread-restore.jsonl',
        sessionFile: 'sessions/thread-restore.jsonl', archived: true, archivedAt: 1000,
        updatedAt: 1, updatedAtMs: 1000,
      },
      shared: null,
    }];
    dashboard.managedSessions.data.totalCount = 1;
    dashboard.managedSessions.data.archivedCount = 1;
    apiMocks.restoreSessionsVisible.mockResolvedValue({
      operationId: 'restore-visible-1',
      selectedCount: 1,
      backups: [{
        backupDir: 'C:\\backups\\restore-visible-1',
        sourceRoot: 'C:\\Users\\alice\\.codex',
        reason: 'restore-visible',
        createdAtMs: 20,
        scope: 'stateOnly',
        trackedDatabaseCount: 1,
        completeSessions: false,
      }],
      deletedThreads: 0,
      deletedSessionFiles: 0,
      removedSessionIndexEntries: 0,
      restoredThreads: 1,
      checkpointCleanup: {
        attemptedCount: 1,
        failedCount: 0,
        reclaimedCount: 1,
        reclaimedBytes: 4096,
        retainedCount: 0,
        warnings: [],
      },
    });
    render(<App loadDashboard={() => Promise.resolve(dashboard)} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话' }));
    fireEvent.click(screen.getByLabelText(/^选择 thread-restore/));
    fireEvent.click(screen.getByRole('button', { name: '恢复可见' }));

    expect(await screen.findByText('操作 ID：restore-visible-1')).toBeTruthy();
    expect(screen.getByText('临时检查点已释放：4.0 KiB')).toBeTruthy();
    expect(screen.queryByText('备份：1')).toBeNull();
    expect(screen.queryByText('备份路径：C:\\backups\\restore-visible-1')).toBeNull();
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
