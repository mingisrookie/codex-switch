import { Channel, invoke } from '@tauri-apps/api/core';
import type {
  AppExitRequestResult,
  AppStatus,
  BackupDeleteReceipt,
  BackupDashboardData,
  CheckpointCleanupReceipt,
  CheckpointStorageStatus,
  CreateFullBackupReceipt,
  CodexHomeStatus,
  CodexProcess,
  DashboardData,
  BackupSummary,
  ChatGptLaunchResult,
  DiagnosticExportReceipt,
  DiagnosticExportFailure,
  DiagnosticExportTarget,
  DiagnosticStatus,
  FrontendDiagnosticInput,
  ManagedSessionInventory,
  MigrationPreflightReport,
  MigrationBackupManifest,
  MigrationCancellationReceipt,
  MigrationPreparationReceipt,
  MigrationApplyReceipt,
  OfflineGcReceipt,
  DowngradeExportReceipt,
  RestoreImportReceipt,
  LegacyBackupReconciliationReceipt,
  PendingRecoveryList,
  ConflictResolutionAction,
  ConflictResolutionReceipt,
  SessionConflictList,
  RelayRuntimeInput,
  MobileContinuityStatus,
  RestoreResult,
  RuntimeStatus,
  RuntimeSwitchResult,
  RuntimeMetadata,
  SessionMutationResult,
  SessionInventory,
  ShadowScanReport,
  SessionStorageControlState,
  SessionStorageInvestigationReceipt,
  SessionSyncProgress,
  SessionSyncResult,
  OperationRecord,
  RuntimeDashboardData,
  RuntimeKind,
  RuntimeSwitchProgress,
  SkillConfigInput,
  SkillId,
  SkillMutationReceipt,
  SkillStatus,
  SessionDashboardData,
  UpdateCheckResult,
  UpdateInstallReceipt,
  UpdateStartupNotice,
} from './types';

const mutationErrorEnvelopePrefix = '__CHATGPT_SWITCH_MUTATION_ERROR_V1__';
const maxMutationErrorMessageBytes = 16 * 1024;
const maxMutationErrorEnvelopeChars = 128 * 1024;
const maxMutationCorrelationIdLength = 160;
const mutationCorrelationIdPattern = /^[A-Za-z0-9](?:[A-Za-z0-9_-]*[A-Za-z0-9])?$/;

export type MutationFailure = Readonly<{
  message: string;
  operationId: string;
}>;

async function invokeMutation<T>(command: string, args?: Record<string, unknown>) {
  try {
    return args === undefined
      ? await invoke<T>(command)
      : await invoke<T>(command, args);
  } catch (reason) {
    throw decodeMutationFailure(reason) ?? reason;
  }
}

function decodeMutationFailure(reason: unknown): MutationFailure | null {
  const encoded = typeof reason === 'string'
    ? reason
    : reason instanceof Error
      ? reason.message
      : null;
  if (!encoded?.startsWith(mutationErrorEnvelopePrefix)) return null;
  const payload = encoded.slice(mutationErrorEnvelopePrefix.length);
  if (!payload || payload.length > maxMutationErrorEnvelopeChars) return null;
  try {
    const candidate = JSON.parse(payload) as unknown;
    if (!candidate || typeof candidate !== 'object' || Array.isArray(candidate)) return null;
    const fields = candidate as Record<string, unknown>;
    const keys = Object.keys(fields);
    if (keys.length !== 2 || !keys.includes('message') || !keys.includes('operationId')) return null;
    if (typeof fields.message !== 'string'
      || new TextEncoder().encode(fields.message).length > maxMutationErrorMessageBytes
      || typeof fields.operationId !== 'string'
      || fields.operationId.length > maxMutationCorrelationIdLength
      || !mutationCorrelationIdPattern.test(fields.operationId)) {
      return null;
    }
    return { message: fields.message, operationId: fields.operationId };
  } catch {
    return null;
  }
}

export function getAppStatus() {
  return invoke<AppStatus>('get_app_status');
}

export function requestAppExit() {
  return invokeMutation<AppExitRequestResult>('request_app_exit');
}

export function getDiagnosticStatus() {
  return invoke<DiagnosticStatus>('get_diagnostic_status');
}

const diagnosticRetryIdPattern = /^diagnostic-export-context-[0-9a-f]{32}$/;

export function normalizeDiagnosticExportFailure(reason: unknown): DiagnosticExportFailure {
  if (reason && typeof reason === 'object') {
    const candidate = reason as { kind?: unknown; message?: unknown; retryId?: unknown };
    const message = typeof candidate.message === 'string' && candidate.message.trim()
      ? candidate.message.slice(0, 512)
      : '诊断导出请求失败';
    if (candidate.kind === 'destination'
      && typeof candidate.retryId === 'string'
      && diagnosticRetryIdPattern.test(candidate.retryId)) {
      return { kind: 'destination', message, retryId: candidate.retryId };
    }
    if (candidate.kind === 'preparation') return { kind: 'preparation', message };
  }
  if (reason instanceof Error && reason.message.trim()) {
    return { kind: 'preparation', message: reason.message.slice(0, 512) };
  }
  if (typeof reason === 'string' && reason.trim()) {
    return { kind: 'preparation', message: reason.slice(0, 512) };
  }
  return { kind: 'preparation', message: '诊断导出请求失败' };
}

async function invokeDiagnosticExport(
  command: string,
  args: Record<string, string>,
) {
  try {
    return await invoke<DiagnosticExportReceipt>(command, args);
  } catch (reason) {
    throw normalizeDiagnosticExportFailure(reason);
  }
}

export function exportDiagnostics(operationId?: string) {
  return invokeDiagnosticExport('export_diagnostics', operationId ? { operationId } : {});
}

export function retryDiagnosticExport(
  retryId: string,
  target: DiagnosticExportTarget,
) {
  return target === 'downloads'
    ? invokeDiagnosticExport('export_diagnostics', { retryId })
    : invokeDiagnosticExport('export_diagnostics_to_diagnostic_directory', { retryId });
}

export function openDiagnosticExport(exportId: string) {
  return invoke<void>('open_diagnostic_export', { exportId });
}

export function openDiagnosticLogDirectory() {
  return invoke<void>('open_diagnostic_log_directory');
}

export function clearDiagnosticLogs() {
  return invoke<void>('clear_diagnostic_logs');
}

export function recordFrontendDiagnostic(input: FrontendDiagnosticInput) {
  return invoke<void>('record_frontend_diagnostic', { input });
}

export function checkForUpdates() {
  return invoke<UpdateCheckResult>('check_for_updates');
}

export function installUpdate() {
  return invokeMutation<UpdateInstallReceipt>('install_update');
}

export function getUpdateStartupNotice() {
  return invoke<UpdateStartupNotice | null>('get_update_startup_notice');
}

export function getSessionStorageStatus() {
  return invoke<ShadowScanReport | null>('get_session_storage_status');
}

export function getSessionStorageControlState() {
  return invoke<SessionStorageControlState>('get_session_storage_control_state');
}

export function setSessionStorageAutomaticCleanup(enabled: boolean) {
  return invokeMutation<SessionStorageControlState>('set_session_storage_automatic_cleanup', { enabled });
}

export function scanSessionStorage() {
  return invoke<ShadowScanReport>('scan_session_storage');
}

export function createSessionStorageInvestigationTask() {
  return invokeMutation<SessionStorageInvestigationReceipt>(
    'create_session_storage_investigation_task',
  );
}

export function openSessionStorageInvestigationTask(taskId: string) {
  return invoke<void>('open_session_storage_investigation_task', { taskId });
}

export function preflightSessionStorageMigration(backupDestination: string) {
  return invokeMutation<MigrationPreflightReport>('preflight_session_storage_migration', {
    backupDestination,
  });
}

export function createSessionStorageMigrationBackup(operationId: string) {
  return invokeMutation<MigrationBackupManifest>('create_session_storage_migration_backup', {
    operationId,
  });
}

export function verifySessionStorageMigrationBackup(operationId: string) {
  return invokeMutation<MigrationBackupManifest>('verify_session_storage_migration_backup', {
    operationId,
  });
}

export function prepareSessionStorageMigration(operationId: string) {
  return invokeMutation<MigrationPreparationReceipt>('prepare_session_storage_migration', {
    operationId,
  });
}

export function cancelSessionStorageMigration(operationId: string) {
  return invokeMutation<MigrationCancellationReceipt>('cancel_session_storage_migration', {
    operationId,
  });
}

export function applySessionStorageMigration(operationId: string) {
  return invokeMutation<MigrationApplyReceipt>('apply_session_storage_migration', {
    operationId,
  });
}

export function runSessionStorageOfflineGc(migrationOperationId: string) {
  return invokeMutation<OfflineGcReceipt>('run_session_storage_offline_gc', {
    migrationOperationId,
  });
}

export function exportSessionStorageDowngrade(
  migrationOperationId: string,
  targetVersion: string,
  destinationRoot: string,
) {
  return invokeMutation<DowngradeExportReceipt>('export_session_storage_downgrade', {
    migrationOperationId,
    targetVersion,
    destinationRoot,
  });
}

export function importSessionStorageDowngrade(
  migrationOperationId: string,
  packageDir: string,
) {
  return invokeMutation<RestoreImportReceipt>('import_session_storage_downgrade', {
    migrationOperationId,
    packageDir,
  });
}

export function reconcileSessionStorageLegacyBackups(migrationOperationId: string) {
  return invokeMutation<LegacyBackupReconciliationReceipt>('reconcile_session_storage_legacy_backups', {
    migrationOperationId,
  });
}

export function listSessionStoragePendingRecovery(migrationOperationId: string) {
  return invoke<PendingRecoveryList>('list_session_storage_pending_recovery', {
    migrationOperationId,
  });
}

export function deferSessionStoragePendingRecovery(
  migrationOperationId: string,
  entryId: string,
) {
  return invokeMutation<PendingRecoveryList>('defer_session_storage_pending_recovery', {
    migrationOperationId,
    entryId,
  });
}

export function restoreSessionStoragePendingRecovery(
  migrationOperationId: string,
  entryId: string,
) {
  return invokeMutation<RestoreImportReceipt>('restore_session_storage_pending_recovery', {
    migrationOperationId,
    entryId,
  });
}

export function listSessionStorageConflicts(migrationOperationId: string) {
  return invoke<SessionConflictList>('list_session_storage_conflicts', {
    migrationOperationId,
  });
}

export function resolveSessionStorageConflict(
  migrationOperationId: string,
  conflictId: string,
  action: ConflictResolutionAction,
) {
  return invokeMutation<ConflictResolutionReceipt>('resolve_session_storage_conflict', {
    migrationOperationId,
    conflictId,
    action,
  });
}

export async function loadDashboard(): Promise<DashboardData> {
  const [
    codexHome,
    sessions,
    managedSessions,
    sessionStorage,
    runtimes,
    runtimeStatus,
    backups,
    operations,
  ] =
    await Promise.allSettled([
      invoke<CodexHomeStatus>('scan_codex_home'),
      invoke<SessionInventory>('scan_sessions'),
      invoke<ManagedSessionInventory>('scan_managed_sessions'),
      invoke<ShadowScanReport | null>('get_session_storage_status'),
      invoke<RuntimeMetadata[]>('list_runtimes'),
      invoke<RuntimeStatus>('scan_runtime_status'),
      invoke<BackupSummary[]>('list_backups'),
      invoke<OperationRecord[]>('list_operation_records', { limit: 20 }),
    ]);
  const [backupStorage] = await Promise.allSettled([
    invoke<CheckpointStorageStatus>('inspect_checkpoint_storage'),
  ]);

  return {
    codexHome: settledDomain(codexHome),
    sessions: settledDomain(sessions),
    managedSessions: settledDomain(managedSessions),
    sessionStorage: settledDomain(sessionStorage),
    runtimes: settledDomain(runtimes),
    runtimeStatus: settledDomain(runtimeStatus),
    backups: settledDomain(backups),
    backupStorage: settledDomain(backupStorage),
    operations: settledDomain(operations),
  };
}

export async function loadRuntimeDashboard(): Promise<RuntimeDashboardData> {
  const [codexHome, sessionStorage, runtimes, runtimeStatus, operations] = await Promise.allSettled([
    invoke<CodexHomeStatus>('scan_codex_home'),
    invoke<ShadowScanReport | null>('get_session_storage_status'),
    invoke<RuntimeMetadata[]>('list_runtimes'),
    invoke<RuntimeStatus>('scan_runtime_status'),
    invoke<OperationRecord[]>('list_operation_records', { limit: 20 }),
  ]);

  return {
    codexHome: settledDomain(codexHome),
    sessionStorage: settledDomain(sessionStorage),
    runtimes: settledDomain(runtimes),
    runtimeStatus: settledDomain(runtimeStatus),
    operations: settledDomain(operations),
  };
}

export async function loadSessionDashboard(): Promise<SessionDashboardData> {
  const [sessions, managedSessions, sessionStorage] = await Promise.allSettled([
    invoke<SessionInventory>('scan_sessions'),
    invoke<ManagedSessionInventory>('scan_managed_sessions'),
    invoke<ShadowScanReport | null>('get_session_storage_status'),
  ]);

  return {
    sessions: settledDomain(sessions),
    managedSessions: settledDomain(managedSessions),
    sessionStorage: settledDomain(sessionStorage),
  };
}

export async function loadBackupDashboard(): Promise<BackupDashboardData> {
  const [backups, operations] = await Promise.allSettled([
    invoke<BackupSummary[]>('list_backups'),
    invoke<OperationRecord[]>('list_operation_records', { limit: 20 }),
  ]);
  const [backupStorage] = await Promise.allSettled([
    invoke<CheckpointStorageStatus>('inspect_checkpoint_storage'),
  ]);
  return {
    backups: settledDomain(backups),
    backupStorage: settledDomain(backupStorage),
    operations: settledDomain(operations),
  };
}

export function loadingDashboard(): DashboardData {
  return {
    codexHome: { status: 'loading' },
    sessions: { status: 'loading' },
    managedSessions: { status: 'loading' },
    sessionStorage: { status: 'loading' },
    runtimes: { status: 'loading' },
    runtimeStatus: { status: 'loading' },
    backups: { status: 'loading' },
    backupStorage: { status: 'loading' },
    operations: { status: 'loading' },
  };
}

export function importPlusRuntime(confirmOverwrite: boolean) {
  return invokeMutation<RuntimeMetadata>('import_plus_runtime', { confirmOverwrite });
}

export function upsertRelayRuntime(input: RelayRuntimeInput) {
  return invokeMutation<RuntimeMetadata>('upsert_relay_runtime', { input });
}

export function listCodexProcesses() {
  return invoke<CodexProcess[]>('list_codex_processes');
}

export function closeCodexProcesses() {
  return invokeMutation<CodexProcess[]>('close_codex_processes');
}

export function switchRuntime(
  runtimeId: RuntimeKind,
  onProgress: (event: RuntimeSwitchProgress) => void,
) {
  const onProgressChannel = new Channel<RuntimeSwitchProgress>(onProgress);
  return invokeMutation<RuntimeSwitchResult>('switch_runtime', {
    runtimeId,
    relayPreference: null,
    onProgress: onProgressChannel,
  });
}

export function launchChatgpt() {
  return invokeMutation<ChatGptLaunchResult>('launch_chatgpt');
}

export function mergeAndRepairSessions(onProgress: (event: SessionSyncProgress) => void) {
  const onProgressChannel = new Channel<SessionSyncProgress>(onProgress);
  return invokeMutation<SessionSyncResult>('merge_and_repair_sessions', { onProgress: onProgressChannel });
}

export function getMobileContinuityStatus() {
  return invoke<MobileContinuityStatus>('get_mobile_continuity_status');
}

export function setMobileContinuityEnabled(enabled: boolean) {
  return invokeMutation<MobileContinuityStatus>('set_mobile_continuity_enabled', { enabled });
}

export function acknowledgeMobileContinuityNotice() {
  return invokeMutation<MobileContinuityStatus>('acknowledge_mobile_continuity_notice');
}

export function publishMobileContinuitySession(threadId: string) {
  return invokeMutation<MobileContinuityStatus>('publish_mobile_continuity_session', { threadId });
}

export function restoreSessionsVisible(ids: string[]) {
  return invokeMutation<SessionMutationResult>('restore_sessions_visible', { ids });
}

export function restoreBackup(backupDir: string) {
  return invokeMutation<RestoreResult>('restore_backup', { backupDir });
}

export function createFullBackup() {
  return invokeMutation<CreateFullBackupReceipt>('create_full_backup');
}

export function deleteBackup(backupDir: string, confirmed: true) {
  return invokeMutation<BackupDeleteReceipt>('delete_backup', { backupDir, confirmed });
}

export function cleanupAutomaticCheckpoints() {
  return invokeMutation<CheckpointCleanupReceipt>('cleanup_automatic_checkpoints');
}

export function listSkills() {
  return invoke<SkillStatus[]>('list_skills');
}

export function installSkill(skillId: SkillId, confirmReplace: boolean) {
  return invokeMutation<SkillMutationReceipt>('install_skill', { skillId, confirmReplace });
}

export function saveSkillConfig(input: SkillConfigInput) {
  return invokeMutation<SkillMutationReceipt>('save_skill_config', { input });
}

function settledDomain<T>(result: PromiseSettledResult<T>) {
  if (result.status === 'fulfilled') {
    return { status: 'ready' as const, data: result.value };
  }
  return {
    status: 'error' as const,
    error: result.reason instanceof Error ? result.reason.message : String(result.reason),
  };
}
