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
  cleanupAutomaticCheckpoints,
  createFullBackup,
  clearDiagnosticLogs,
  deleteBackup,
  createSessionStorageMigrationBackup,
  createSessionStorageInvestigationTask,
  getAppStatus,
  getDiagnosticStatus,
  getMobileContinuityStatus,
  getSessionStorageStatus,
  getSessionStorageControlState,
  getUpdateStartupNotice,
  importPlusRuntime,
  exportDiagnostics,
  exportSessionStorageDowngrade,
  importSessionStorageDowngrade,
  reconcileSessionStorageLegacyBackups,
  listSessionStoragePendingRecovery,
  deferSessionStoragePendingRecovery,
  restoreSessionStoragePendingRecovery,
  normalizeDiagnosticExportFailure,
  retryDiagnosticExport,
  installUpdate,
  installSkill,
  launchChatgpt,
  loadBackupDashboard,
  listSkills,
  loadDashboard,
  loadRuntimeDashboard,
  loadSessionDashboard,
  listSessionStorageConflicts,
  openSessionStorageInvestigationTask,
  resolveSessionStorageConflict,
  openDiagnosticExport,
  openDiagnosticLogDirectory,
  requestAppExit,
  recordFrontendDiagnostic,
  acknowledgeMobileContinuityNotice,
  applySessionStorageMigration,
  cancelSessionStorageMigration,
  publishMobileContinuitySession,
  preflightSessionStorageMigration,
  prepareSessionStorageMigration,
  runSessionStorageOfflineGc,
  setMobileContinuityEnabled,
  setSessionStorageAutomaticCleanup,
  saveSkillConfig,
  scanSessionStorage,
  switchRuntime,
  mergeAndRepairSessions,
  upsertRelayRuntime,
  verifySessionStorageMigrationBackup,
} from './api';
import type { BackupSummary, RuntimeSwitchProgress } from './types';

describe('dashboard API', () => {
  beforeEach(() => invoke.mockReset());

  it('requests a backend-owned process exit without frontend paths or flags', async () => {
    invoke.mockResolvedValue({ scheduled: true });

    await expect(requestAppExit()).resolves.toEqual({ scheduled: true });
    expect(invoke).toHaveBeenCalledWith('request_app_exit');
  });

  it('recovers correlated mutation failures without changing the business message', async () => {
    const relayFailure = {
      message: 'relay save failed: original message',
      operationId: 'save-relay-1780000000000-42-1',
    };
    invoke.mockRejectedValueOnce(
      `__CHATGPT_SWITCH_MUTATION_ERROR_V1__${JSON.stringify(relayFailure)}`,
    );

    await expect(upsertRelayRuntime({
      baseUrl: 'https://relay.example.com/v1',
      model: 'example-model',
      apiKey: 'placeholder-key',
    })).rejects.toEqual(relayFailure);
    expect(invoke).toHaveBeenCalledWith('upsert_relay_runtime', {
      input: {
        baseUrl: 'https://relay.example.com/v1',
        model: 'example-model',
        apiKey: 'placeholder-key',
      },
    });

    const skillFailure = {
      message: 'skill install failed unchanged',
      operationId: 'install-skill-attempt-1780000000000-42-2',
    };
    invoke.mockRejectedValueOnce(
      `__CHATGPT_SWITCH_MUTATION_ERROR_V1__${JSON.stringify(skillFailure)}`,
    );

    await expect(installSkill('image2', false)).rejects.toEqual(skillFailure);
    expect(invoke).toHaveBeenLastCalledWith('install_skill', {
      skillId: 'image2',
      confirmReplace: false,
    });
  });

  it('preserves bare and malformed mutation rejection strings for compatibility', async () => {
    const legacy = 'legacy backend failure';
    invoke.mockRejectedValueOnce(legacy);
    await expect(upsertRelayRuntime({
      baseUrl: 'https://relay.example.com/v1',
      model: 'example-model',
      apiKey: 'placeholder-key',
    })).rejects.toBe(legacy);

    const malformed = `__CHATGPT_SWITCH_MUTATION_ERROR_V1__${JSON.stringify({
      message: 'must not be trusted as correlated',
      operationId: '../outside',
    })}`;
    invoke.mockRejectedValueOnce(malformed);
    await expect(installUpdate()).rejects.toBe(malformed);
  });

  it('uses typed diagnostic commands without exposing arbitrary paths or raw errors', async () => {
    invoke.mockResolvedValue(undefined);

    await getDiagnosticStatus();
    await exportDiagnostics();
    await exportDiagnostics('sync-1');
    await retryDiagnosticExport('diagnostic-export-context-aabbccddeeff00112233445566778899', 'downloads');
    await retryDiagnosticExport(
      'diagnostic-export-context-aabbccddeeff00112233445566778899',
      'diagnosticDirectory',
    );
    await openDiagnosticExport('export-1');
    await openDiagnosticLogDirectory();
    await clearDiagnosticLogs();
    await recordFrontendDiagnostic({
      level: 'error',
      component: 'frontend',
      eventKind: 'unhandledError',
      errorCode: 'frontend.unhandled_error',
      safeMessage: '前端发生未处理异常',
    });

    expect(invoke).toHaveBeenNthCalledWith(1, 'get_diagnostic_status');
    expect(invoke).toHaveBeenNthCalledWith(2, 'export_diagnostics', {});
    expect(invoke).toHaveBeenNthCalledWith(3, 'export_diagnostics', { operationId: 'sync-1' });
    expect(invoke).toHaveBeenNthCalledWith(4, 'export_diagnostics', {
      retryId: 'diagnostic-export-context-aabbccddeeff00112233445566778899',
    });
    expect(invoke).toHaveBeenNthCalledWith(5, 'export_diagnostics_to_diagnostic_directory', {
      retryId: 'diagnostic-export-context-aabbccddeeff00112233445566778899',
    });
    expect(invoke).toHaveBeenNthCalledWith(6, 'open_diagnostic_export', { exportId: 'export-1' });
    expect(invoke).toHaveBeenNthCalledWith(7, 'open_diagnostic_log_directory');
    expect(invoke).toHaveBeenNthCalledWith(8, 'clear_diagnostic_logs');
    expect(invoke).toHaveBeenNthCalledWith(9, 'record_frontend_diagnostic', {
      input: {
        level: 'error',
        component: 'frontend',
        eventKind: 'unhandledError',
        errorCode: 'frontend.unhandled_error',
        safeMessage: '前端发生未处理异常',
      },
    });
  });

  it('preserves only a valid typed destination retry context', async () => {
    const failure = {
      kind: 'destination',
      message: 'Downloads is unavailable',
      retryId: 'diagnostic-export-context-aabbccddeeff00112233445566778899',
    };
    invoke.mockRejectedValueOnce(failure);

    await expect(exportDiagnostics('switch-1')).rejects.toEqual(failure);
    expect(normalizeDiagnosticExportFailure({
      ...failure,
      retryId: 'C:\\Users\\alice\\Downloads',
    })).toEqual({
      kind: 'preparation',
      message: '诊断导出请求失败',
    });
    expect(normalizeDiagnosticExportFailure(new Error('worker failed'))).toEqual({
      kind: 'preparation',
      message: 'worker failed',
    });
  });

  it('uses fixed mobile-continuity commands and passes only typed settings or thread ids', async () => {
    invoke.mockResolvedValue({ enabled: true, items: [] });

    await getMobileContinuityStatus();
    await setMobileContinuityEnabled(false);
    await acknowledgeMobileContinuityNotice();
    await publishMobileContinuitySession('11111111-1111-4111-8111-111111111111');

    expect(invoke).toHaveBeenNthCalledWith(1, 'get_mobile_continuity_status');
    expect(invoke).toHaveBeenNthCalledWith(2, 'set_mobile_continuity_enabled', {
      enabled: false,
    });
    expect(invoke).toHaveBeenNthCalledWith(3, 'acknowledge_mobile_continuity_notice');
    expect(invoke).toHaveBeenNthCalledWith(4, 'publish_mobile_continuity_session', {
      threadId: '11111111-1111-4111-8111-111111111111',
    });
  });

  it('keeps successful domains when one dashboard scan fails', async () => {
    invoke.mockImplementation((command: string) => {
      if (command === 'scan_managed_sessions') {
        return Promise.reject(new Error('managed scan failed'));
      }
      const values: Record<string, unknown> = {
        scan_codex_home: { root: 'C:\\Users\\alice\\.codex' },
        scan_sessions: { threadCount: 4, sessionJsonlCount: 3 },
        get_session_storage_status: null,
        list_runtimes: [],
        scan_runtime_status: {
          activeRuntimeId: null,
          confidence: 'unknown',
          authMode: null,
          modelProvider: null,
          detectedAtMs: 1,
        },
        list_backups: [],
        inspect_checkpoint_storage: {
          totalCount: 0,
          totalBytes: 0,
          reclaimableCount: 0,
          reclaimableBytes: 0,
          retainedCount: 0,
          warnings: [],
          lastCleanup: null,
        },
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
    expect(dashboard.sessionStorage).toMatchObject({ status: 'ready', data: null });
    expect(dashboard.runtimes).toMatchObject({ status: 'ready', data: [] });
    expect(dashboard.runtimeStatus).toMatchObject({ status: 'ready' });
    expect(dashboard.backups).toMatchObject({ status: 'ready', data: [] });
    expect(dashboard.backupStorage).toMatchObject({ status: 'ready' });
    expect(dashboard.operations).toMatchObject({ status: 'ready', data: [] });
    expect(invoke).toHaveBeenCalledTimes(9);
    expect(invoke).toHaveBeenCalledWith('list_operation_records', { limit: 20 });
  });

  it('refreshes runtime-facing domains without scanning sessions', async () => {
    invoke.mockImplementation((command: string) => Promise.resolve({
      get_session_storage_status: null,
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
    expect(dashboard.sessionStorage).toMatchObject({ status: 'ready', data: null });
    expect(invoke).toHaveBeenCalledTimes(5);
    expect(invoke).not.toHaveBeenCalledWith('scan_sessions');
    expect(invoke).not.toHaveBeenCalledWith('scan_managed_sessions');
    expect(invoke).not.toHaveBeenCalledWith('list_backups');
  });

  it('loads session domains independently when the session page becomes visible', async () => {
    invoke.mockImplementation((command: string) => Promise.resolve({
      scan_sessions: { threadCount: 4, sessionJsonlCount: 3 },
      scan_managed_sessions: { totalCount: 4, archivedCount: 0, sessions: [] },
      get_session_storage_status: null,
    }[command]));

    const dashboard = await loadSessionDashboard();

    expect(dashboard.sessions).toMatchObject({ status: 'ready' });
    expect(dashboard.sessionStorage).toMatchObject({ status: 'ready', data: null });
    expect(invoke).toHaveBeenCalledTimes(3);
    expect(invoke).not.toHaveBeenCalledWith('list_backups');
    expect(invoke).not.toHaveBeenCalledWith('list_runtimes');
  });

  it('uses separate typed commands for the cached status and explicit shadow scan', async () => {
    invoke.mockResolvedValue(null);

    await getSessionStorageStatus();
    await scanSessionStorage();

    expect(invoke).toHaveBeenNthCalledWith(1, 'get_session_storage_status');
    expect(invoke).toHaveBeenNthCalledWith(2, 'scan_session_storage');
  });

  it('creates and opens only backend-owned sanitized investigation tasks', async () => {
    invoke.mockResolvedValue(undefined);

    await createSessionStorageInvestigationTask();
    await openSessionStorageInvestigationTask('codex-investigation-1-safe');

    expect(invoke).toHaveBeenNthCalledWith(1, 'create_session_storage_investigation_task');
    expect(invoke).toHaveBeenNthCalledWith(2, 'open_session_storage_investigation_task', {
      taskId: 'codex-investigation-1-safe',
    });
  });

  it('loads canonical storage control state and persists the automatic cleanup setting', async () => {
    invoke.mockResolvedValue({ canonicalReady: true, automaticCleanupEnabled: false });

    await getSessionStorageControlState();
    await setSessionStorageAutomaticCleanup(false);

    expect(invoke).toHaveBeenNthCalledWith(1, 'get_session_storage_control_state');
    expect(invoke).toHaveBeenNthCalledWith(2, 'set_session_storage_automatic_cleanup', {
      enabled: false,
    });
  });

  it('passes the selected local backup directory to migration preflight', async () => {
    invoke.mockResolvedValue({ readyForBackup: false });

    await preflightSessionStorageMigration('E:\\codex-switch-backups');

    expect(invoke).toHaveBeenCalledWith('preflight_session_storage_migration', {
      backupDestination: 'E:\\codex-switch-backups',
    });
  });

  it('binds migration backup creation to the durable operation ID', async () => {
    invoke.mockResolvedValue({ status: 'integrityVerified' });

    await createSessionStorageMigrationBackup('session-migration-1');

    expect(invoke).toHaveBeenCalledWith('create_session_storage_migration_backup', {
      operationId: 'session-migration-1',
    });
  });

  it('binds native migration backup verification to the durable operation ID', async () => {
    invoke.mockResolvedValue({ status: 'runtimeVerified' });

    await verifySessionStorageMigrationBackup('session-migration-1');

    expect(invoke).toHaveBeenCalledWith('verify_session_storage_migration_backup', {
      operationId: 'session-migration-1',
    });
  });

  it('binds migration apply planning to the durable operation ID', async () => {
    invoke.mockResolvedValue({ preparedSessionCount: 2 });

    await prepareSessionStorageMigration('session-migration-1');

    expect(invoke).toHaveBeenCalledWith('prepare_session_storage_migration', {
      operationId: 'session-migration-1',
    });
  });

  it('binds migration apply to the durable operation ID', async () => {
    invoke.mockResolvedValue({ validated: true });

    await applySessionStorageMigration('session-migration-1');

    expect(invoke).toHaveBeenCalledWith('apply_session_storage_migration', {
      operationId: 'session-migration-1',
    });
  });

  it('binds precommit migration cancellation to the durable operation ID', async () => {
    invoke.mockResolvedValue({ stagingDiscarded: true });

    await cancelSessionStorageMigration('session-migration-1');

    expect(invoke).toHaveBeenCalledWith('cancel_session_storage_migration', {
      operationId: 'session-migration-1',
    });
  });

  it('binds offline cleanup to the committed migration backup', async () => {
    invoke.mockResolvedValue({ validated: true, deletedCount: 1 });

    await runSessionStorageOfflineGc('session-migration-1');

    expect(invoke).toHaveBeenCalledWith('run_session_storage_offline_gc', {
      migrationOperationId: 'session-migration-1',
    });
  });

  it('binds an explicit v0.2 export to migration, exact version, and isolated destination', async () => {
    invoke.mockResolvedValue({ structurallyVerified: true });

    await exportSessionStorageDowngrade(
      'session-migration-1',
      'v0.2.7',
      'E:\\codex-switch-downgrade',
    );

    expect(invoke).toHaveBeenCalledWith('export_session_storage_downgrade', {
      migrationOperationId: 'session-migration-1',
      targetVersion: 'v0.2.7',
      destinationRoot: 'E:\\codex-switch-downgrade',
    });
  });

  it('binds downgrade re-upgrade import to its migration and isolated package', async () => {
    invoke.mockResolvedValue({ validated: true });

    await importSessionStorageDowngrade(
      'session-migration-1',
      'E:\\codex-switch-downgrade-v0.2.7',
    );

    expect(invoke).toHaveBeenCalledWith('import_session_storage_downgrade', {
      migrationOperationId: 'session-migration-1',
      packageDir: 'E:\\codex-switch-downgrade-v0.2.7',
    });
  });

  it('binds legacy backup reconciliation and pending recovery actions to the committed migration', async () => {
    invoke.mockResolvedValue({ entries: [] });

    await reconcileSessionStorageLegacyBackups('session-migration-1');
    await listSessionStoragePendingRecovery('session-migration-1');
    await deferSessionStoragePendingRecovery('session-migration-1', 'entry-1');
    await restoreSessionStoragePendingRecovery('session-migration-1', 'entry-2');

    expect(invoke).toHaveBeenNthCalledWith(1, 'reconcile_session_storage_legacy_backups', {
      migrationOperationId: 'session-migration-1',
    });
    expect(invoke).toHaveBeenNthCalledWith(2, 'list_session_storage_pending_recovery', {
      migrationOperationId: 'session-migration-1',
    });
    expect(invoke).toHaveBeenNthCalledWith(3, 'defer_session_storage_pending_recovery', {
      migrationOperationId: 'session-migration-1',
      entryId: 'entry-1',
    });
    expect(invoke).toHaveBeenNthCalledWith(4, 'restore_session_storage_pending_recovery', {
      migrationOperationId: 'session-migration-1',
      entryId: 'entry-2',
    });
  });

  it('lists conflict summaries by committed migration operation ID', async () => {
    invoke.mockResolvedValue({ migrationOperationId: 'session-migration-1', conflicts: [] });

    await listSessionStorageConflicts('session-migration-1');

    expect(invoke).toHaveBeenCalledWith('list_session_storage_conflicts', {
      migrationOperationId: 'session-migration-1',
    });
  });

  it('submits an explicit conflict action without exposing session content', async () => {
    invoke.mockResolvedValue({ status: 'deferred', validated: true });

    await resolveSessionStorageConflict(
      'session-migration-1',
      `conflict-${'a'.repeat(64)}`,
      'defer',
    );

    expect(invoke).toHaveBeenCalledWith('resolve_session_storage_conflict', {
      migrationOperationId: 'session-migration-1',
      conflictId: `conflict-${'a'.repeat(64)}`,
      action: 'defer',
    });
  });

  it('loads expensive backup verification only through its explicit loader', async () => {
    invoke.mockImplementation((command: string) => Promise.resolve(command === 'inspect_checkpoint_storage'
      ? {
          totalCount: 2,
          totalBytes: 4096,
          reclaimableCount: 1,
          reclaimableBytes: 2048,
          retainedCount: 1,
          warnings: [],
          lastCleanup: null,
        }
      : []));

    const dashboard = await loadBackupDashboard();

    expect(dashboard.backups).toMatchObject({ status: 'ready', data: [] });
    expect(dashboard.backupStorage).toMatchObject({
      status: 'ready',
      data: { reclaimableCount: 1 },
    });
    expect(dashboard.operations).toMatchObject({ status: 'ready', data: [] });
    expect(invoke).toHaveBeenCalledTimes(3);
    expect(invoke).toHaveBeenCalledWith('list_backups');
    expect(invoke).toHaveBeenCalledWith('inspect_checkpoint_storage');
    expect(invoke).toHaveBeenCalledWith('list_operation_records', { limit: 20 });
  });

  it('waits for backup migration/listing before acquiring the checkpoint scan guard', async () => {
    let resolveBackups!: (value: BackupSummary[]) => void;
    const backups = new Promise<BackupSummary[]>((resolve) => {
      resolveBackups = resolve;
    });
    invoke.mockImplementation((command: string) => {
      if (command === 'list_backups') return backups;
      if (command === 'list_operation_records') return Promise.resolve([]);
      if (command === 'inspect_checkpoint_storage') {
        return Promise.resolve({
          totalCount: 0,
          totalBytes: 0,
          reclaimableCount: 0,
          reclaimableBytes: 0,
          retainedCount: 0,
          warnings: [],
          lastCleanup: null,
        });
      }
      return Promise.resolve(undefined);
    });

    const pending = loadBackupDashboard();
    await Promise.resolve();
    expect(invoke).not.toHaveBeenCalledWith('inspect_checkpoint_storage');

    resolveBackups([]);
    await pending;

    expect(invoke).toHaveBeenCalledWith('inspect_checkpoint_storage');
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

  it('uses the fixed command for a typed full backup receipt', async () => {
    invoke.mockResolvedValue({
      operationId: 'backup-1',
      backups: [
        {
          backupDir: 'C:\\backups\\manual-current',
          sourceRoot: 'C:\\Users\\alice\\.codex',
          reason: 'manual-full-backup',
          createdAtMs: 10,
          scope: 'full',
          trackedDatabaseCount: 1,
          completeSessions: true,
        },
        {
          backupDir: 'C:\\backups\\manual-shared',
          sourceRoot: 'C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\shared-sessions',
          reason: 'manual-full-backup',
          createdAtMs: 11,
          scope: 'full',
          trackedDatabaseCount: 1,
          completeSessions: true,
        },
      ],
      warnings: [],
    });

    const receipt = await createFullBackup();

    expect(invoke).toHaveBeenCalledWith('create_full_backup');
    expect(receipt).toMatchObject({
      operationId: 'backup-1',
      backups: [{ scope: 'full' }, { scope: 'full' }],
    });
  });

  it('passes the verified backup path and explicit confirmation when deleting a backup', async () => {
    invoke.mockResolvedValue({
      operationId: 'delete-backup-1',
      backupDir: 'C:\\backups\\manual-current',
      reclaimedBytes: 4096,
      warnings: [],
    });

    const receipt = await deleteBackup('C:\\backups\\manual-current', true);

    expect(invoke).toHaveBeenCalledWith('delete_backup', {
      backupDir: 'C:\\backups\\manual-current',
      confirmed: true,
    });
    expect(receipt).toMatchObject({
      operationId: 'delete-backup-1',
      reclaimedBytes: 4096,
    });
  });

  it('uses the fixed command for automatic checkpoint cleanup', async () => {
    invoke.mockResolvedValue({
      operationId: 'cleanup-1',
      attemptedCount: 2,
      failedCount: 0,
      reclaimedCount: 2,
      reclaimedBytes: 4096,
      retainedCount: 3,
      warnings: [],
    });

    const receipt = await cleanupAutomaticCheckpoints();

    expect(invoke).toHaveBeenCalledWith('cleanup_automatic_checkpoints');
    expect(receipt).toMatchObject({
      operationId: 'cleanup-1',
      attemptedCount: 2,
      failedCount: 0,
      reclaimedCount: 2,
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
    const events: RuntimeSwitchProgress[] = [];

    await switchRuntime('relay', (event) => events.push(event));

    const payload = invoke.mock.calls[0][1] as {
      runtimeId: string;
      onProgress: { onmessage: (event: RuntimeSwitchProgress) => void };
    };
    expect(invoke).toHaveBeenCalledWith('switch_runtime', {
      runtimeId: 'relay',
      relayPreference: null,
      onProgress: expect.any(Channel),
    });
    payload.onProgress.onmessage({
      phase: 'failed',
      timestampMs: 10,
      outcome: 'failedBeforeWrite',
      reason: 'standaloneWriterActive',
    });
    expect(events).toEqual([expect.objectContaining({
      phase: 'failed',
      reason: 'standaloneWriterActive',
    })]);
  });

  it('runs manual full sync once with a progress channel and no dry-run command', async () => {
    invoke.mockResolvedValue({ operationId: 'sync-1' });
    const phases: string[] = [];

    await mergeAndRepairSessions((event) => phases.push(event.phase));

    const payload = invoke.mock.calls[0][1] as {
      onProgress: { onmessage: (event: { phase: string; timestampMs: number }) => void };
    };
    expect(invoke).toHaveBeenCalledWith('merge_and_repair_sessions', {
      onProgress: expect.any(Channel),
    });
    payload.onProgress.onmessage({ phase: 'reconciling', timestampMs: 10 });
    expect(phases).toEqual(['reconciling']);
    expect(invoke).not.toHaveBeenCalledWith('dry_run_all_sessions');
  });

  it('retries ChatGPT launch through the fixed backend command without path arguments', async () => {
    invoke.mockResolvedValue({ status: 'alreadyRunning', message: null });

    await launchChatgpt();

    expect(invoke).toHaveBeenCalledWith('launch_chatgpt');
  });
});
