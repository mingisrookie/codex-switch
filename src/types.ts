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
  runtimes: RuntimeMetadata[];
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
