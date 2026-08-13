import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';

import {
  SessionStorageManagementPage,
  type SessionStorageManagementDependencies,
} from './SessionStorageManagementPage';
import type {
  MigrationBackupManifest,
  MigrationPreflightReport,
  PendingRecoveryList,
  SessionConflictList,
  SessionStorageControlState,
  ShadowScanReport,
} from './types';

const report: ShadowScanReport = {
  schemaVersion: 1,
  scanId: 'scan-1',
  generatedAtMs: 1,
  status: 'migrationAvailable',
  migrationRequired: true,
  deletionEnabled: false,
  summary: {
    schemaVersion: 1,
    onlineScanOnly: true,
    nonAtomicAcrossDatabases: true,
    logicalSessionCount: 3,
    canonicalCandidateCount: 2,
    duplicatedSessionCount: 1,
    conflictSessionCount: 0,
    highConfidenceCopyCount: 1,
    sessionFileCount: 3,
    sessionBytes: 300,
    potentialReclaimBytes: 100,
    markerFileCount: 1,
    runtimeDatabaseCount: 1,
    backupDatabaseCount: 0,
    runtimeReferenceCount: 2,
    missingRuntimeReferenceCount: 0,
    mismatchedRuntimeReferenceCount: 0,
    cacheHitCount: 0,
    cacheMissCount: 3,
    stableFileCount: 3,
    turnContextCount: 0,
    resolvedTurnProvenanceCount: 0,
    historicalUnknownTurnCount: 0,
    incompleteTurnProvenanceCount: 0,
    relationCounts: { equal: 1, equalExceptProvider: 0, prefix: 0, divergent: 0, unknown: 0 },
  },
  issues: [],
};

const pendingControl: SessionStorageControlState = {
  schemaVersion: 1,
  canonicalReady: false,
  automaticCleanupEnabled: true,
  onlineDeletionEnabled: false,
  reclaimedBytes: 4096,
};

const committedControl: SessionStorageControlState = {
  ...pendingControl,
  canonicalReady: true,
  migrationOperationId: 'migration-1',
  migrationPreparedAtMs: 2,
};

function dependencies(overrides: Partial<SessionStorageManagementDependencies> = {}) {
  const emptyConflicts: SessionConflictList = {
    migrationOperationId: 'migration-1',
    conflicts: [],
  };
  const emptyPendingRecovery: PendingRecoveryList = {
    migrationOperationId: 'migration-1',
    entries: [],
    expiredPackageCount: 0,
    invalidPackageCount: 0,
  };
  return {
    getControlState: vi.fn(async () => pendingControl),
    getStatus: vi.fn(async () => ({ ...report, scanId: 'scan-default-next' })),
    setAutomaticCleanup: vi.fn(async (enabled: boolean) => ({
      ...pendingControl,
      automaticCleanupEnabled: enabled,
    })),
    scan: vi.fn(async () => report),
    createInvestigationTask: vi.fn(),
    openInvestigationTask: vi.fn(),
    preflight: vi.fn(),
    createBackup: vi.fn(),
    verifyBackup: vi.fn(),
    prepareMigration: vi.fn(),
    cancelMigration: vi.fn(),
    applyMigration: vi.fn(),
    runOfflineGc: vi.fn(),
    listConflicts: vi.fn(async () => emptyConflicts),
    resolveConflict: vi.fn(),
    exportDowngrade: vi.fn(),
    importDowngrade: vi.fn(),
    reconcileLegacyBackups: vi.fn(),
    listPendingRecovery: vi.fn(async () => emptyPendingRecovery),
    deferPendingRecovery: vi.fn(),
    restorePendingRecovery: vi.fn(),
    ...overrides,
  } as SessionStorageManagementDependencies;
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

describe('SessionStorageManagementPage', () => {
  it('creates and opens a sanitized local Codex investigation task for out-of-scope issues', async () => {
    const investigationReport: ShadowScanReport = {
      ...report,
      status: 'reviewRequired',
      issues: [{ code: 'missingRuntimeReference', count: 2 }],
    };
    const createInvestigationTask = vi.fn(async () => ({
      taskId: 'codex-investigation-1-safe',
      issueCount: 2,
      databaseCount: 3,
      displayPath: '[app-data]/codex-switch/session-storage-v1/codex-investigations/task/TASK.md',
      taskSha256: 'a'.repeat(64),
    }));
    const openInvestigationTask = vi.fn(async () => undefined);
    const deps = dependencies({ createInvestigationTask, openInvestigationTask });
    render(
      <SessionStorageManagementPage
        active
        initialReport={investigationReport}
        dependencies={deps}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: '交给 Codex 排查' }));

    expect(await screen.findByText(/\[app-data\]\/codex-switch\/session-storage-v1/))
      .not.toBeNull();
    expect(createInvestigationTask).toHaveBeenCalledTimes(1);
    expect(screen.queryByText(/C:\\Users\\private/)).toBeNull();
    expect(screen.queryByText(/fixture message body/)).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: '打开任务目录' }));
    await waitFor(() => expect(openInvestigationTask)
      .toHaveBeenCalledWith('codex-investigation-1-safe'));
  });

  it('shows the complete preflight inventory and keeps backup blocked by safety findings', async () => {
    const preflight: MigrationPreflightReport = {
      schemaVersion: 1,
      operationId: 'migration-preflight',
      generatedAtMs: 1,
      canonicalSessionCount: 2,
      sessionFileCount: 12,
      providerCopyCount: 7,
      conflictCount: 2,
      anomalyCount: 1,
      estimatedReclaimBytes: 4096,
      backupSourceBytes: 8192,
      requiredBackupBytes: 16384,
      availableBackupBytes: 1024,
      backupDestination: '[local path]',
      blockers: ['insufficientBackupSpace'],
      readyForBackup: false,
      plan: {
        schemaVersion: 1,
        operationId: 'migration-preflight',
        generatedAtMs: 1,
        canonicalRoot: '[local path]',
        inventoryFingerprint: 'a'.repeat(64),
        sessions: [],
        conflicts: [],
        databases: [],
        unclassifiedFileCount: 0,
        invalidMarkerCount: 0,
        missingRuntimeReferenceCount: 0,
        mismatchedRuntimeReferenceCount: 0,
      },
    };
    const deps = dependencies({ preflight: vi.fn(async () => preflight) });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    fireEvent.change(screen.getByLabelText('完整备份目录'), { target: { value: 'E:\\backup' } });
    fireEvent.click(screen.getByRole('button', { name: '开始只读预检' }));

    expect(await screen.findByText('12')).not.toBeNull();
    expect(screen.getByText(/insufficientBackupSpace/)).not.toBeNull();
    expect((screen.getByRole('button', { name: /创建完整备份/ }) as HTMLButtonElement).disabled)
      .toBe(true);
    expect(deps.preflight).toHaveBeenCalledWith('E:\\backup');
  });

  it('persists the automatic cleanup choice while keeping online deletion scan-only', async () => {
    const setAutomaticCleanup = vi.fn(async () => ({
      ...committedControl,
      automaticCleanupEnabled: false,
    }));
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      setAutomaticCleanup,
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    const toggle = await screen.findByRole('checkbox', { name: /自动清理/ });
    expect((toggle as HTMLInputElement).checked).toBe(true);
    fireEvent.click(toggle);

    await waitFor(() => expect(setAutomaticCleanup).toHaveBeenCalledWith(false));
    expect(screen.getByText(/在线阶段固定只扫描/)).not.toBeNull();
    expect(screen.getByText('仅扫描')).not.toBeNull();
    expect(screen.getAllByText(/关闭只停止 provider 副本自动 GC|仅停止 provider 副本自动 GC/).length)
      .toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText(/7 天隐私\/恢复生命周期仍继续/).length)
      .toBeGreaterThanOrEqual(1);
  });

  it('keeps explicit offline cleanup available when automatic provider GC is disabled', async () => {
    const disabledControl = {
      ...committedControl,
      automaticCleanupEnabled: false,
    };
    const runOfflineGc = vi.fn(async () => ({
      operationId: 'manual-offline-gc',
      candidateCount: 0,
      deletedCount: 0,
      reclaimedBytes: 0,
      validated: true,
    }));
    const deps = dependencies({
      getControlState: vi.fn(async () => disabledControl),
      runOfflineGc,
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    await screen.findByText('Canonical 已就绪');
    fireEvent.click(screen.getByRole('checkbox', { name: '所有 Codex 写入进程已关闭' }));
    const button = screen.getByRole('button', { name: '执行离线安全清理' });
    expect((button as HTMLButtonElement).disabled).toBe(false);
    fireEvent.click(button);

    await waitFor(() => expect(runOfflineGc).toHaveBeenCalledTimes(1));
  });

  it('joins the backend shadow after a successful mutation without waiting or issuing a foreground scan', async () => {
    const freshReport: ShadowScanReport = {
      ...report,
      scanId: 'scan-after-offline-gc',
      generatedAtMs: 2,
      summary: {
        ...report.summary,
        canonicalCandidateCount: 9,
        highConfidenceCopyCount: 0,
      },
    };
    const freshStatus = deferred<ShadowScanReport | null>();
    const getStatus = vi.fn()
      .mockResolvedValueOnce(report)
      .mockImplementationOnce(() => freshStatus.promise);
    const runOfflineGc = vi.fn(async () => ({
      operationId: 'offline-gc-1',
      candidateCount: 1,
      deletedCount: 1,
      reclaimedBytes: 100,
      validated: true,
    }));
    const scan = vi.fn(async () => {
      throw new Error('a session storage shadow scan is already running');
    });
    const onBusyChange = vi.fn();
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      getStatus,
      runOfflineGc,
      scan,
    });
    render(
      <SessionStorageManagementPage
        active
        initialReport={report}
        dependencies={deps}
        onBusyChange={onBusyChange}
      />,
    );

    await screen.findByText('Canonical 已就绪');
    fireEvent.click(screen.getByRole('checkbox', { name: '所有 Codex 写入进程已关闭' }));
    fireEvent.click(screen.getByRole('button', { name: '执行离线安全清理' }));

    expect(await screen.findByText(/离线清理完成/)).not.toBeNull();
    expect(scan).not.toHaveBeenCalled();
    expect(onBusyChange).toHaveBeenLastCalledWith(null);
    expect(screen.queryByRole('alert')).toBeNull();
    await waitFor(() => expect(getStatus).toHaveBeenCalledTimes(2));

    await act(async () => {
      freshStatus.resolve(freshReport);
      await freshStatus.promise;
    });

    await waitFor(() => {
      expect(screen.getByText('Canonical 会话').parentElement?.textContent).toContain('9');
    });
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('joins an already-running manual shadow scan instead of surfacing it as an error', async () => {
    const freshReport: ShadowScanReport = {
      ...report,
      scanId: 'scan-manual-joined',
      generatedAtMs: 3,
      summary: { ...report.summary, canonicalCandidateCount: 7 },
    };
    const scan = vi.fn(async () => {
      throw new Error('a session storage shadow scan is already running');
    });
    const getStatus = vi.fn(async () => freshReport);
    const deps = dependencies({ scan, getStatus });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    fireEvent.click(screen.getByRole('button', { name: '重新扫描' }));

    expect(await screen.findByText(/已加入本次扫描/)).not.toBeNull();
    await waitFor(() => {
      expect(screen.getByText('Canonical 会话').parentElement?.textContent).toContain('7');
    });
    expect(scan).toHaveBeenCalledTimes(1);
    expect(getStatus).toHaveBeenCalled();
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('shows only metadata summaries and offers overwrite only when a newer version is reliable', async () => {
    const conflictList: SessionConflictList = {
      migrationOperationId: 'migration-1',
      conflicts: [
        {
          conflictId: `conflict-${'a'.repeat(64)}`,
          deferred: false,
          currentMessageCount: 4,
          candidateMessageCount: 6,
          currentAddedMessageCount: 0,
          candidateAddedMessageCount: 2,
          currentLastMessageAt: '2026-08-11T00:00:00Z',
          candidateLastMessageAt: '2026-08-12T00:00:00Z',
          currentProvider: 'openai',
          candidateProvider: 'openai_custom',
          currentOrigin: 'canonicalHome',
          candidateOrigin: 'shared',
          relation: 'divergent',
          newerVersion: 'candidate',
          defaultOverwrite: false,
        },
        {
          conflictId: `conflict-${'b'.repeat(64)}`,
          deferred: false,
          currentMessageCount: 3,
          candidateMessageCount: 3,
          currentAddedMessageCount: 1,
          candidateAddedMessageCount: 1,
          currentOrigin: 'canonicalHome',
          candidateOrigin: 'shared',
          relation: 'unknown',
          defaultOverwrite: false,
        },
      ],
    };
    const resolveConflict = vi.fn(async () => ({
      operationId: 'resolution-1',
      migrationOperationId: 'migration-1',
      conflictId: conflictList.conflicts[0].conflictId,
      status: 'resolved' as const,
      chosenVersion: 'candidate' as const,
      canonicalUpdated: true,
      databaseViewCount: 1,
      recoveryExpiresAtMs: 3,
      validated: true,
    }));
    const listConflicts = vi.fn(async () => conflictList);
    const listPendingRecovery = vi.fn(async (): Promise<PendingRecoveryList> => ({
      migrationOperationId: 'migration-1',
      entries: [],
      expiredPackageCount: 0,
      invalidPackageCount: 0,
    }));
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      listConflicts,
      listPendingRecovery,
      resolveConflict,
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    const overwrite = await screen.findByRole('button', { name: '使用较新版本覆盖' });
    expect(screen.getByText('已回收')).not.toBeNull();
    expect(screen.getByText('4.0 KiB')).not.toBeNull();
    expect(screen.getByText('时间不可靠，不提供覆盖推荐')).not.toBeNull();
    expect(screen.queryByText(/fixture message body/)).toBeNull();
    fireEvent.click(overwrite);

    await waitFor(() => expect(resolveConflict).toHaveBeenCalledWith(
      'migration-1',
      conflictList.conflicts[0].conflictId,
      'useNewer',
    ));
    await waitFor(() => expect(listConflicts.mock.calls.length).toBeGreaterThanOrEqual(2));
    await waitFor(() => expect(listPendingRecovery.mock.calls.length).toBeGreaterThanOrEqual(2));
  });

  it('persists conflict defer through the backend and reloads the persisted summary', async () => {
    const conflictId = `conflict-${'c'.repeat(64)}`;
    let storedDeferred = false;
    const listConflicts = vi.fn(async (): Promise<SessionConflictList> => ({
      migrationOperationId: 'migration-1',
      conflicts: [{
        conflictId,
        deferred: storedDeferred,
        currentMessageCount: 4,
        candidateMessageCount: 6,
        currentAddedMessageCount: 0,
        candidateAddedMessageCount: 2,
        currentLastMessageAt: '2026-08-11T00:00:00Z',
        candidateLastMessageAt: '2026-08-12T00:00:00Z',
        currentProvider: 'openai',
        candidateProvider: 'openai_custom',
        currentOrigin: 'canonicalHome',
        candidateOrigin: 'shared',
        relation: 'divergent',
        newerVersion: 'candidate',
        defaultOverwrite: false,
      }],
    }));
    const resolveConflict = vi.fn(async (
      _migrationOperationId: string,
      _conflictId: string,
      action: 'defer' | 'useNewer',
    ) => {
      storedDeferred = action === 'defer';
      return {
        migrationOperationId: 'migration-1',
        conflictId,
        status: 'deferred' as const,
        canonicalUpdated: false,
        databaseViewCount: 0,
        validated: true,
      };
    });
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      listConflicts,
      resolveConflict,
    });
    const firstRender = render(
      <SessionStorageManagementPage active initialReport={report} dependencies={deps} />,
    );

    fireEvent.click(await screen.findByRole('button', { name: '暂不覆盖' }));

    await waitFor(() => expect(resolveConflict).toHaveBeenCalledWith(
      'migration-1',
      conflictId,
      'defer',
    ));
    await waitFor(() => expect(listConflicts.mock.calls.length).toBeGreaterThanOrEqual(2));
    const deferredButton = await screen.findByRole('button', { name: '已暂不覆盖' });
    expect((deferredButton as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getByText(/已持久化“暂不覆盖”/)).not.toBeNull();

    firstRender.unmount();
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    const persistedButton = await screen.findByRole('button', { name: '已暂不覆盖' });
    expect((persistedButton as HTMLButtonElement).disabled).toBe(true);
    expect(resolveConflict).toHaveBeenCalledTimes(1);
  });

  it('runs the gated migration flow in order and reports busy state to the app close guard', async () => {
    const preflight: MigrationPreflightReport = {
      schemaVersion: 1,
      operationId: 'migration-1',
      generatedAtMs: 1,
      canonicalSessionCount: 2,
      sessionFileCount: 3,
      providerCopyCount: 1,
      conflictCount: 0,
      anomalyCount: 0,
      estimatedReclaimBytes: 100,
      backupSourceBytes: 300,
      requiredBackupBytes: 600,
      availableBackupBytes: 1_000,
      backupDestination: '[local path]',
      blockers: [],
      readyForBackup: true,
      plan: {
        schemaVersion: 1,
        operationId: 'migration-1',
        generatedAtMs: 1,
        canonicalRoot: '[local path]',
        inventoryFingerprint: 'a'.repeat(64),
        sessions: [],
        conflicts: [],
        databases: [],
        unclassifiedFileCount: 0,
        invalidMarkerCount: 0,
        missingRuntimeReferenceCount: 0,
        mismatchedRuntimeReferenceCount: 0,
      },
    };
    const createdBackup: MigrationBackupManifest = {
      schemaVersion: 1,
      operationId: 'migration-1',
      createdAtMs: 2,
      expiresAtMs: 3,
      backupDir: 'E:\\backup\\migration-1',
      status: 'integrityVerified',
      entries: [],
    };
    const verifiedBackup: MigrationBackupManifest = {
      ...createdBackup,
      status: 'runtimeVerified',
      isolatedRestoreVerifiedAtMs: 3,
      runtimeVerification: {
        expectedSessionCount: 2,
        listedSessionCount: 2,
        resumedSessionCount: 2,
        toolSessionCount: 1,
        toolRoundTripVerified: true,
        verifiedAtMs: 3,
      },
    };
    const getControlState = vi.fn()
      .mockResolvedValueOnce(pendingControl)
      .mockResolvedValue(committedControl);
    const onBusyChange = vi.fn();
    const deps = dependencies({
      getControlState,
      preflight: vi.fn(async () => preflight),
      createBackup: vi.fn(async () => createdBackup),
      verifyBackup: vi.fn(async () => verifiedBackup),
      prepareMigration: vi.fn(async () => ({
        operationId: 'migration-1',
        preparedSessionCount: 2,
        preparedDatabaseCount: 1,
        conflictCount: 0,
        preparedBytes: 300,
      })),
      applyMigration: vi.fn(async () => ({
        operationId: 'migration-1',
        canonicalCreatedCount: 1,
        canonicalReplacedCount: 0,
        databaseViewCount: 1,
        conflictCount: 0,
        validated: true,
      })),
    });
    render(
      <SessionStorageManagementPage
        active
        initialReport={report}
        dependencies={deps}
        onBusyChange={onBusyChange}
      />,
    );

    fireEvent.change(screen.getByLabelText('完整备份目录'), { target: { value: 'E:\\backup' } });
    fireEvent.click(screen.getByRole('button', { name: '开始只读预检' }));
    await screen.findByText(/预检通过/);

    fireEvent.click(screen.getByRole('button', { name: /创建完整备份/ }));
    await screen.findByText(/必须继续通过真实 Codex/);
    fireEvent.click(screen.getByRole('button', { name: /真实恢复验证/ }));
    await screen.findByText(/已通过隔离恢复/);
    fireEvent.click(screen.getByRole('button', { name: /生成原子计划/ }));
    await screen.findByText(/提交前仍可取消/);

    fireEvent.click(screen.getByRole('checkbox', { name: /我已关闭 Codex Desktop/ }));
    fireEvent.click(screen.getByRole('button', { name: '提交迁移并验证' }));

    expect(await screen.findByText(/迁移已通过数据校验/)).not.toBeNull();
    expect(screen.getByText('Canonical 已就绪')).not.toBeNull();
    expect(deps.applyMigration).toHaveBeenCalledWith('migration-1');
    expect(deps.scan).not.toHaveBeenCalled();
    expect(deps.getStatus).toHaveBeenCalled();
    expect(onBusyChange).toHaveBeenCalledWith('提交会话存储迁移');
    expect(onBusyChange).toHaveBeenLastCalledWith(null);
  });

  it('cancels before apply and clears all migration gates without touching canonical state', async () => {
    const preflight: MigrationPreflightReport = {
      schemaVersion: 1,
      operationId: 'cancel-1',
      generatedAtMs: 1,
      canonicalSessionCount: 1,
      sessionFileCount: 1,
      providerCopyCount: 0,
      conflictCount: 0,
      anomalyCount: 0,
      estimatedReclaimBytes: 0,
      backupSourceBytes: 10,
      requiredBackupBytes: 20,
      availableBackupBytes: 100,
      backupDestination: '[local path]',
      blockers: [],
      readyForBackup: true,
      plan: {
        schemaVersion: 1,
        operationId: 'cancel-1',
        generatedAtMs: 1,
        canonicalRoot: '[local path]',
        inventoryFingerprint: 'b'.repeat(64),
        sessions: [],
        conflicts: [],
        databases: [],
        unclassifiedFileCount: 0,
        invalidMarkerCount: 0,
        missingRuntimeReferenceCount: 0,
        mismatchedRuntimeReferenceCount: 0,
      },
    };
    const deps = dependencies({
      preflight: vi.fn(async () => preflight),
      cancelMigration: vi.fn(async () => ({
        operationId: 'cancel-1',
        backupRetained: false,
        stagingDiscarded: true,
      })),
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    fireEvent.change(screen.getByLabelText('完整备份目录'), { target: { value: 'E:\\backup' } });
    fireEvent.click(screen.getByRole('button', { name: '开始只读预检' }));
    await screen.findByRole('button', { name: '取消迁移' });
    fireEvent.click(screen.getByRole('button', { name: '取消迁移' }));

    expect(await screen.findByText(/canonical 数据未切换/)).not.toBeNull();
    expect(screen.getByRole('button', { name: '开始只读预检' })).not.toBeNull();
    expect(deps.cancelMigration).toHaveBeenCalledWith('cancel-1');
    expect(deps.applyMigration).not.toHaveBeenCalled();
  });

  it('shows only pending-recovery metadata and restores only safe relations with writers closed', async () => {
    const pending: PendingRecoveryList = {
      migrationOperationId: 'migration-1',
      entries: [
        {
          entryId: 'entry-private-source-path',
          threadId: 'thread-missing-12345678',
          relation: 'missingFromCanonical',
          status: 'pending',
          sourceBackupId: 'backup-abcdefgh',
          sourceBackupCreatedAtMs: 1,
          candidateMessageCount: 9,
          currentMessageCount: 0,
          candidateAddedMessageCount: 9,
          currentAddedMessageCount: 0,
          candidateLastMessageAt: '2026-08-12T01:02:03Z',
          candidateProvider: 'openai_custom',
          payloadBytes: 2048,
          expiresAtMs: Date.now() + 60_000,
          restoreAllowed: true,
        },
        {
          entryId: 'entry-divergent',
          threadId: 'thread-divergent-87654321',
          relation: 'divergent',
          status: 'pending',
          sourceBackupId: 'backup-ijklmnop',
          sourceBackupCreatedAtMs: 1,
          candidateMessageCount: 7,
          currentMessageCount: 6,
          candidateAddedMessageCount: 1,
          currentAddedMessageCount: 1,
          candidateProvider: 'relay',
          currentProvider: 'openai',
          payloadBytes: 1024,
          expiresAtMs: Date.now() + 60_000,
          restoreAllowed: false,
        },
      ],
      expiredPackageCount: 0,
      invalidPackageCount: 0,
    };
    const restorePendingRecovery = vi.fn(async () => ({
      operationId: 'restore-1',
      packageOperationId: 'entry-private-source-path',
      targetVersion: 'v0.3.0',
      packageDir: 'C:\\private\\must-not-render',
      scannedSessionCount: 1,
      unchangedSessionCount: 0,
      currentAheadSessionCount: 0,
      importedNewSessionCount: 1,
      importedExtensionCount: 0,
      conflictCount: 0,
      unclassifiedRecoveryCount: 0,
      unclassifiedRecoveryBytes: 0,
      unclassifiedRecoveryPaths: [],
      anomalyCount: 0,
      databaseViewCount: 1,
      importedBytes: 2048,
      recoveryExpiresAtMs: Date.now() + 60_000,
      validated: true,
    }));
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      listPendingRecovery: vi.fn(async () => pending),
      restorePendingRecovery,
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    expect(await screen.findByText('主库缺失')).not.toBeNull();
    expect(screen.getByText('真实分叉')).not.toBeNull();
    expect(screen.getByText(/真实分叉，默认不覆盖/)).not.toBeNull();
    expect(screen.queryByText('entry-private-source-path')).toBeNull();
    expect(screen.queryByText('C:\\private\\must-not-render')).toBeNull();
    expect(screen.queryByText(/fixture message body/)).toBeNull();

    const restoreButton = screen.getByRole('button', { name: '恢复到 canonical' });
    expect((restoreButton as HTMLButtonElement).disabled).toBe(true);
    expect(screen.getAllByRole('button', { name: '恢复到 canonical' })).toHaveLength(1);
    fireEvent.click(screen.getByRole('checkbox', { name: '所有 Codex 写入进程已关闭' }));
    fireEvent.click(restoreButton);

    await waitFor(() => expect(restorePendingRecovery).toHaveBeenCalledWith(
      'migration-1',
      'entry-private-source-path',
    ));
  });

  it('reconciles legacy backups only after the offline writer gate and refreshes the inbox', async () => {
    const listPendingRecovery = vi.fn(async () => ({
      migrationOperationId: 'migration-1',
      entries: [],
      expiredPackageCount: 0,
      invalidPackageCount: 0,
    }));
    const reconcileLegacyBackups = vi.fn(async () => ({
      operationId: 'legacy-reconcile-1',
      migrationOperationId: 'migration-1',
      scannedBackupCount: 3,
      deletedBackupCount: 2,
      retainedBackupCount: 1,
      unreadableBackupCount: 0,
      pendingRecoveryCount: 1,
      conflictCount: 0,
      reclaimedBytes: 4096,
      validated: true,
    }));
    const listConflicts = vi.fn(async (): Promise<SessionConflictList> => ({
      migrationOperationId: 'migration-1',
      conflicts: [],
    }));
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      listConflicts,
      listPendingRecovery,
      reconcileLegacyBackups,
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    const reconcileButton = await screen.findByRole('button', { name: '验证并整理旧备份' });
    expect((reconcileButton as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole('checkbox', { name: '所有 Codex 写入进程已关闭' }));
    fireEvent.click(reconcileButton);

    expect(reconcileLegacyBackups).not.toHaveBeenCalled();
    const confirmationHeading = screen.getByRole('heading', { name: '确认提取并删除旧备份' });
    expect(confirmationHeading).not.toBeNull();
    expect(document.activeElement).toBe(confirmationHeading);
    expect(screen.getByText(/把独有会话提取到待恢复区，并删除已证明可安全整理的原备份/))
      .not.toBeNull();
    fireEvent.click(screen.getByRole('button', { name: '确认提取并删除可安全整理的旧备份' }));

    await waitFor(() => expect(reconcileLegacyBackups).toHaveBeenCalledWith('migration-1'));
    expect(await screen.findByText(/独有会话先进入 7 天待恢复区/)).not.toBeNull();
    expect(screen.queryByRole('heading', { name: '确认提取并删除旧备份' })).toBeNull();
    await waitFor(() => expect(document.activeElement).toBe(reconcileButton));
    expect(listPendingRecovery.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(listConflicts.mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it('surfaces retained unclassified restore payloads without exposing absolute paths', async () => {
    const importDowngrade = vi.fn(async () => ({
      operationId: 'restore-unclassified-1',
      packageOperationId: 'downgrade-package-1',
      targetVersion: 'v0.2.7',
      packageDir: 'C:\\private\\package-must-not-render',
      scannedSessionCount: 3,
      unchangedSessionCount: 1,
      currentAheadSessionCount: 0,
      importedNewSessionCount: 1,
      importedExtensionCount: 0,
      conflictCount: 0,
      unclassifiedRecoveryCount: 2,
      unclassifiedRecoveryBytes: 1536,
      unclassifiedRecoveryPaths: [
        'session-storage-v1/recovery/unclassified/payload-1.bin',
        'C:\\private\\payload-must-not-render.bin',
      ],
      anomalyCount: 2,
      databaseViewCount: 1,
      importedBytes: 2048,
      recoveryExpiresAtMs: Date.now() + 60_000,
      validated: true,
    }));
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      importDowngrade,
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    await screen.findByText('Canonical 已就绪');
    fireEvent.click(screen.getByRole('checkbox', { name: '所有 Codex 写入进程已关闭' }));
    fireEvent.change(screen.getByLabelText('使用过的降级包目录'), {
      target: { value: 'E:\\isolated\\downgrade-v0.2.7' },
    });
    fireEvent.click(screen.getByRole('button', { name: '导入旧版本新增会话' }));

    await waitFor(() => expect(importDowngrade).toHaveBeenCalledWith(
      'migration-1',
      'E:\\isolated\\downgrade-v0.2.7',
    ));
    await waitFor(() => {
      expect(screen.getAllByText(/未分类载荷已保留/).length).toBeGreaterThanOrEqual(2);
    });
    expect(screen.getAllByText(/不会自动到期删除，需要交给 Codex 排查/).length)
      .toBeGreaterThanOrEqual(2);
    expect(screen.getAllByText('session-storage-v1/recovery/unclassified/payload-1.bin').length)
      .toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText('[absolute path omitted]').length).toBeGreaterThanOrEqual(1);
    expect(screen.queryByText(/C:\\private\\payload-must-not-render/)).toBeNull();
    expect(screen.queryByText(/C:\\private\\package-must-not-render/)).toBeNull();
  });

  it('exports only after the offline gate and distinguishes native from target-old runtime verification', async () => {
    const exportDowngrade = vi.fn(async () => ({
      operationId: 'downgrade-1',
      target: {
        version: 'v0.2.0',
        band: 'a' as const,
        runtimeBundleRequired: false,
        incrementalIndexRequired: false,
        relaySessionViewSupported: false,
        mobileContinuityRequired: false,
      },
      packageDir: 'E:\\isolated\\downgrade-v0.2.0',
      logicalSessionCount: 3,
      sessionFileCount: 3,
      conflictBranchCount: 0,
      recoveryPayloadCount: 0,
      packageBytes: 4096,
      containsCredentials: true,
      structurallyVerified: true,
      nativeRuntimeVerified: true,
      targetRuntimeVerificationRequired: true,
    }));
    const deps = dependencies({
      getControlState: vi.fn(async () => committedControl),
      exportDowngrade,
    });
    render(<SessionStorageManagementPage active initialReport={report} dependencies={deps} />);

    const exportButton = await screen.findByRole('button', { name: '生成隔离降级包' });
    fireEvent.change(screen.getByLabelText('目标旧版本'), { target: { value: 'v0.2.0' } });
    fireEvent.change(screen.getByLabelText('隔离包目标目录'), {
      target: { value: 'E:\\isolated' },
    });
    expect((exportButton as HTMLButtonElement).disabled).toBe(true);

    fireEvent.click(screen.getByRole('checkbox', { name: '所有 Codex 写入进程已关闭' }));
    fireEvent.click(exportButton);

    await waitFor(() => expect(exportDowngrade)
      .toHaveBeenCalledWith('migration-1', 'v0.2.0', 'E:\\isolated'));
    expect(await screen.findByText(/当前原生 Codex 的隔离列表\/读取\/恢复验证/))
      .not.toBeNull();
    expect(screen.getByText(/仍须用目标旧版本完成真实列表、恢复和继续验证/))
      .not.toBeNull();
    expect(screen.getAllByText(/生成后不要移动|该包绑定生成目录/).length)
      .toBeGreaterThanOrEqual(1);
    expect((screen.getByLabelText('使用过的降级包目录') as HTMLInputElement).value)
      .toBe('E:\\isolated\\downgrade-v0.2.0');
  });
});
