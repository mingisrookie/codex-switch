import { invoke } from '@tauri-apps/api/core';
import type {
  CodexHomeStatus,
  CodexProcess,
  DashboardData,
  RelayRuntimeInput,
  RuntimeMetadata,
  SessionInventory,
} from './types';

export async function loadDashboard(): Promise<DashboardData> {
  try {
    const [codexHome, sessions, runtimes] = await Promise.all([
      invoke<CodexHomeStatus>('scan_codex_home'),
      invoke<SessionInventory>('scan_sessions'),
      invoke<RuntimeMetadata[]>('list_runtimes'),
    ]);
    return { codexHome, sessions, runtimes };
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
    runtimes: [],
  };
}
