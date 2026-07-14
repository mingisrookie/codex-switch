import { invoke } from '@tauri-apps/api/core';
import type {
  AppStatus,
  CodexHomeStatus,
  CodexProcess,
  DashboardData,
  BackupSummary,
  ManagedSessionInventory,
  RelayRuntimeInput,
  RestoreResult,
  RuntimeStatus,
  RuntimeSwitchResult,
  RuntimeMetadata,
  SessionMutationResult,
  SessionInventory,
  SessionSyncResult,
  AllSessionsDryRun,
  OperationRecord,
  SkillConfigInput,
  SkillId,
  SkillMutationReceipt,
  SkillStatus,
  UpdateCheckResult,
} from './types';

export function getAppStatus() {
  return invoke<AppStatus>('get_app_status');
}

export function checkForUpdates() {
  return invoke<UpdateCheckResult>('check_for_updates');
}

export function openUpdatePage() {
  return invoke<void>('open_update_page');
}

export async function loadDashboard(): Promise<DashboardData> {
  const [codexHome, sessions, managedSessions, runtimes, runtimeStatus, backups, operations] =
    await Promise.allSettled([
      invoke<CodexHomeStatus>('scan_codex_home'),
      invoke<SessionInventory>('scan_sessions'),
      invoke<ManagedSessionInventory>('scan_managed_sessions'),
      invoke<RuntimeMetadata[]>('list_runtimes'),
      invoke<RuntimeStatus>('scan_runtime_status'),
      invoke<BackupSummary[]>('list_backups'),
      invoke<OperationRecord[]>('list_operation_records', { limit: 20 }),
    ]);

  return {
    codexHome: settledDomain(codexHome),
    sessions: settledDomain(sessions),
    managedSessions: settledDomain(managedSessions),
    runtimes: settledDomain(runtimes),
    runtimeStatus: settledDomain(runtimeStatus),
    backups: settledDomain(backups),
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

export function switchRuntime(runtimeId: string) {
  return invoke<RuntimeSwitchResult>('switch_runtime', { runtimeId });
}

export function syncAllSessions() {
  return invoke<SessionSyncResult>('sync_all_sessions');
}

export function dryRunAllSessions() {
  return invoke<AllSessionsDryRun>('dry_run_all_sessions');
}

export function verifyRelayRuntime() {
  return invoke<RuntimeMetadata>('test_relay_connection');
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
