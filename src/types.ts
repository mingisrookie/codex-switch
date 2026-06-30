export type FileStatus = {
  path: string;
  exists: boolean;
  bytes: number | null;
};

export type AuthSummary = {
  authMode: string | null;
  topLevelKeys: string[];
  hasTokensObject: boolean;
};

export type CodexHomeStatus = {
  root: string;
  sqliteHome: string;
  authJson: FileStatus;
  configToml: FileStatus;
  stateDb: FileStatus;
  logsDb: FileStatus;
  codexDevDb: FileStatus;
  sessionsDir: FileStatus;
  sessionJsonlCount: number;
  authSummary: AuthSummary | null;
};

export type ThreadRecord = {
  id: string;
  rolloutPath: string | null;
  title: string | null;
  preview: string | null;
  modelProvider: string | null;
  archived: boolean;
  archivedAt: number | null;
  updatedAt: number | null;
  updatedAtMs: number | null;
};

export type SessionFileRecord = {
  path: string;
  sessionId: string | null;
  bytes: number;
};

export type SessionInventory = {
  home: string;
  threadCount: number;
  sessionJsonlCount: number;
  threads: ThreadRecord[];
  sessionFiles: SessionFileRecord[];
};

export type DashboardData = {
  codexHome: CodexHomeStatus;
  sessions: SessionInventory;
  managedSessions: ManagedSessionInventory;
  runtimes: RuntimeMetadata[];
};

export type ManagedSessionScope = 'current' | 'shared' | 'both' | 'unknown';

export type ManagedSessionLocation = {
  home: string;
  rolloutPath: string | null;
  sessionFile: string | null;
  archived: boolean;
  archivedAt: number | null;
  updatedAt: number | null;
  updatedAtMs: number | null;
};

export type ManagedSessionRecord = {
  id: string;
  title: string | null;
  preview: string | null;
  modelProvider: string | null;
  updatedAt: number | null;
  updatedAtMs: number | null;
  archived: boolean;
  archivedAt: number | null;
  scope: ManagedSessionScope;
  current: ManagedSessionLocation | null;
  shared: ManagedSessionLocation | null;
};

export type ManagedSessionInventory = {
  currentHome: string;
  sharedHome: string;
  totalCount: number;
  archivedCount: number;
  sessions: ManagedSessionRecord[];
};

export type SessionMutationResult = {
  selectedCount: number;
  backups: unknown[];
  deletedThreads: number;
  deletedSessionFiles: number;
  removedSessionIndexEntries: number;
  restoredThreads: number;
};

export type RuntimeKind = 'plus' | 'relay';

export type RuntimeMetadata = {
  id: string;
  name: string;
  kind: RuntimeKind;
  baseUrl: string | null;
  model: string | null;
  createdAtMs: number;
  lastUsedAtMs: number | null;
};

export type CodexProcess = {
  imageName: string;
  pid: number;
};

export type RelayRuntimeInput = {
  baseUrl: string;
  apiKey: string;
  model: string;
};
