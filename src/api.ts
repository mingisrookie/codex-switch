import { invoke } from '@tauri-apps/api/core';
import type {
  CodexHomeStatus,
  CodexProcess,
  DashboardData,
  ManagedSessionInventory,
  RelayRuntimeInput,
  RuntimeMetadata,
  SessionMutationResult,
  SessionInventory,
} from './types';

export async function loadDashboard(): Promise<DashboardData> {
  try {
    const [codexHome, sessions, managedSessions, runtimes] = await Promise.all([
      invoke<CodexHomeStatus>('scan_codex_home'),
      invoke<SessionInventory>('scan_sessions'),
      invoke<ManagedSessionInventory>('scan_managed_sessions'),
      invoke<RuntimeMetadata[]>('list_runtimes'),
    ]);
    return { codexHome, sessions, managedSessions, runtimes };
  } catch {
    return fallbackDashboard();
  }
}

export function importPlusRuntime() {
  return invoke<RuntimeMetadata>('import_plus_runtime');
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
  return invoke('switch_runtime', { runtimeId });
}

export function syncAllSessions() {
  return invoke('sync_all_sessions');
}

export function deleteManagedSessions(ids: string[], confirmUnarchived: boolean) {
  return invoke<SessionMutationResult>('delete_managed_sessions', { ids, confirmUnarchived });
}

export function restoreSessionsVisible(ids: string[]) {
  return invoke<SessionMutationResult>('restore_sessions_visible', { ids });
}

function fallbackDashboard(): DashboardData {
  const codexHome = '%USERPROFILE%\\.codex';

  return {
    codexHome: {
      root: codexHome,
      sqliteHome: codexHome,
      authJson: { path: 'auth.json', exists: false, bytes: null },
      configToml: { path: 'config.toml', exists: false, bytes: null },
      stateDb: { path: 'state_5.sqlite', exists: false, bytes: null },
      logsDb: { path: 'logs_2.sqlite', exists: false, bytes: null },
      codexDevDb: { path: 'sqlite/codex-dev.db', exists: false, bytes: null },
      sessionsDir: { path: 'sessions', exists: false, bytes: null },
      sessionJsonlCount: 0,
      authSummary: null,
    },
    sessions: {
      home: codexHome,
      threadCount: 0,
      sessionJsonlCount: 0,
      threads: [],
      sessionFiles: [],
    },
    managedSessions: {
      currentHome: codexHome,
      sharedHome: '%APPDATA%\\codex-switch\\shared-sessions',
      totalCount: 0,
      archivedCount: 0,
      sessions: [],
    },
    runtimes: [],
  };
}
