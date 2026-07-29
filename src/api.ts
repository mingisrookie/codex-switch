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
  ManagedSessionInventory,
  RelayRuntimeInput,
  RelaySwitchPreference,
  MobileContinuityStatus,
  RestoreResult,
  RuntimeStatus,
  RuntimeSwitchResult,
  RuntimeMetadata,
  SessionMutationResult,
  SessionInventory,
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

export function getAppStatus() {
  return invoke<AppStatus>('get_app_status');
}

export function requestAppExit() {
  return invoke<AppExitRequestResult>('request_app_exit');
}

export function checkForUpdates() {
  return invoke<UpdateCheckResult>('check_for_updates');
}

export function installUpdate() {
  return invoke<UpdateInstallReceipt>('install_update');
}

export function getUpdateStartupNotice() {
  return invoke<UpdateStartupNotice | null>('get_update_startup_notice');
}

export async function loadDashboard(): Promise<DashboardData> {
  const [
    codexHome,
    sessions,
    managedSessions,
    runtimes,
    runtimeStatus,
    backups,
    operations,
  ] =
    await Promise.allSettled([
      invoke<CodexHomeStatus>('scan_codex_home'),
      invoke<SessionInventory>('scan_sessions'),
      invoke<ManagedSessionInventory>('scan_managed_sessions'),
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
    runtimes: settledDomain(runtimes),
    runtimeStatus: settledDomain(runtimeStatus),
    backups: settledDomain(backups),
    backupStorage: settledDomain(backupStorage),
    operations: settledDomain(operations),
  };
}

export async function loadRuntimeDashboard(): Promise<RuntimeDashboardData> {
  const [codexHome, runtimes, runtimeStatus, operations] = await Promise.allSettled([
    invoke<CodexHomeStatus>('scan_codex_home'),
    invoke<RuntimeMetadata[]>('list_runtimes'),
    invoke<RuntimeStatus>('scan_runtime_status'),
    invoke<OperationRecord[]>('list_operation_records', { limit: 20 }),
  ]);

  return {
    codexHome: settledDomain(codexHome),
    runtimes: settledDomain(runtimes),
    runtimeStatus: settledDomain(runtimeStatus),
    operations: settledDomain(operations),
  };
}

export async function loadSessionDashboard(): Promise<SessionDashboardData> {
  const [sessions, managedSessions] = await Promise.allSettled([
    invoke<SessionInventory>('scan_sessions'),
    invoke<ManagedSessionInventory>('scan_managed_sessions'),
  ]);

  return {
    sessions: settledDomain(sessions),
    managedSessions: settledDomain(managedSessions),
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
    runtimes: { status: 'loading' },
    runtimeStatus: { status: 'loading' },
    backups: { status: 'loading' },
    backupStorage: { status: 'loading' },
    operations: { status: 'loading' },
  };
}

export function importPlusRuntime(confirmOverwrite: boolean) {
  return invoke<RuntimeMetadata>('import_plus_runtime', { confirmOverwrite });
}

export function upsertRelayRuntime(input: RelayRuntimeInput) {
  return invoke<RuntimeMetadata>('upsert_relay_runtime', { input });
}

export function listCodexProcesses() {
  return invoke<CodexProcess[]>('list_codex_processes');
}

export function closeCodexProcesses() {
  return invoke<CodexProcess[]>('close_codex_processes');
}

export function switchRuntime(
  runtimeId: RuntimeKind,
  onProgress: (event: RuntimeSwitchProgress) => void,
  relayPreference: RelaySwitchPreference | null = null,
) {
  const onProgressChannel = new Channel<RuntimeSwitchProgress>(onProgress);
  return invoke<RuntimeSwitchResult>('switch_runtime', {
    runtimeId,
    relayPreference,
    onProgress: onProgressChannel,
  });
}

export function launchChatgpt() {
  return invoke<ChatGptLaunchResult>('launch_chatgpt');
}

export function syncAllSessions(onProgress: (event: SessionSyncProgress) => void) {
  const onProgressChannel = new Channel<SessionSyncProgress>(onProgress);
  return invoke<SessionSyncResult>('sync_all_sessions', { onProgress: onProgressChannel });
}

export function verifyRelayRuntime() {
  return invoke<RuntimeMetadata>('test_relay_connection');
}

export function getMobileContinuityStatus() {
  return invoke<MobileContinuityStatus>('get_mobile_continuity_status');
}

export function setMobileContinuityEnabled(enabled: boolean) {
  return invoke<MobileContinuityStatus>('set_mobile_continuity_enabled', { enabled });
}

export function acknowledgeMobileContinuityNotice() {
  return invoke<MobileContinuityStatus>('acknowledge_mobile_continuity_notice');
}

export function publishMobileContinuitySession(threadId: string) {
  return invoke<MobileContinuityStatus>('publish_mobile_continuity_session', { threadId });
}

export function deleteManagedSessions(ids: string[], confirmed: boolean) {
  return invoke<SessionMutationResult>('delete_managed_sessions', { ids, confirmed });
}

export function restoreSessionsVisible(ids: string[]) {
  return invoke<SessionMutationResult>('restore_sessions_visible', { ids });
}

export function restoreBackup(backupDir: string) {
  return invoke<RestoreResult>('restore_backup', { backupDir });
}

export function createFullBackup() {
  return invoke<CreateFullBackupReceipt>('create_full_backup');
}

export function deleteBackup(backupDir: string, confirmed: true) {
  return invoke<BackupDeleteReceipt>('delete_backup', { backupDir, confirmed });
}

export function cleanupAutomaticCheckpoints() {
  return invoke<CheckpointCleanupReceipt>('cleanup_automatic_checkpoints');
}

export function listSkills() {
  return invoke<SkillStatus[]>('list_skills');
}

export function installSkill(skillId: SkillId, confirmReplace: boolean) {
  return invoke<SkillMutationReceipt>('install_skill', { skillId, confirmReplace });
}

export function saveSkillConfig(input: SkillConfigInput) {
  return invoke<SkillMutationReceipt>('save_skill_config', { input });
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
