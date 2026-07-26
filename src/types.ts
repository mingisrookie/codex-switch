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

export type DomainState<T> =
  | { status: 'loading' }
  | { status: 'ready'; data: T }
  | { status: 'error'; error: string };

export type DashboardData = {
  codexHome: DomainState<CodexHomeStatus>;
  sessions: DomainState<SessionInventory>;
  managedSessions: DomainState<ManagedSessionInventory>;
  runtimes: DomainState<RuntimeMetadata[]>;
  runtimeStatus: DomainState<RuntimeStatus>;
  backups: DomainState<BackupSummary[]>;
  backupStorage: DomainState<CheckpointStorageStatus>;
  operations: DomainState<OperationRecord[]>;
};

export type RuntimeDashboardData = Pick<
  DashboardData,
  'codexHome' | 'runtimes' | 'runtimeStatus' | 'operations'
>;

export type SessionDashboardData = Pick<
  DashboardData,
  'sessions' | 'managedSessions'
>;

export type BackupDashboardData = Pick<DashboardData, 'backups' | 'backupStorage' | 'operations'>;

export type AppStatus = {
  appName: string;
  version: string;
  phase: string;
  codexHome: string;
};

export type UpdateCheckResult = {
  currentVersion: string;
  latestVersion: string;
  updateAvailable: boolean;
  releaseNotes: string | null;
  checkedAtMs: number;
};

export type UpdateInstallReceipt = {
  fromVersion: string;
  toVersion: string;
  downloadedBytes: number;
  sha256: string;
  restarting: boolean;
};

export type UpdateStartupNotice = {
  status: 'updated' | 'rolledBack';
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
  operationId?: string;
  selectedCount: number;
  backups: BackupReceiptSummary[];
  deletedThreads: number;
  deletedSessionFiles: number;
  removedSessionIndexEntries: number;
  restoredThreads: number;
  rolledBack?: boolean;
  warnings?: string[];
  checkpointCleanup?: CheckpointCleanupSummary;
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
  lastVerifiedAtMs: number | null;
};

export type RuntimeConfidence = 'exact' | 'mode' | 'unknown';

export type RuntimeStatus = {
  activeRuntimeId: string | null;
  confidence: RuntimeConfidence;
  authMode: string | null;
  modelProvider: string | null;
  detectedAtMs: number;
};

export type CodexProcess = {
  imageName: string;
  pid: number;
  parentPid: number;
};

export type RelayRuntimeInput = {
  baseUrl: string;
  apiKey: string;
  model: string;
};

export type SkillId = 'image2' | 'grokSearch';

export type SkillState =
  | 'missing'
  | 'current'
  | 'updateAvailable'
  | 'drifted'
  | 'unmanaged'
  | 'invalid';

export type SkillStatus = {
  id: SkillId;
  displayName: string;
  description: string;
  installedPath: string;
  state: SkillState;
  bundledVersion: string;
  installedVersion: string | null;
  canInstall: boolean;
  canUpdate: boolean;
  baseUrl: string;
  credentialConfigured: boolean;
  restartRequired: boolean;
  message: string;
};

export type SkillConfigInput = {
  skillId: SkillId;
  baseUrl: string;
  apiKey: string;
};

export type SkillMutationAction = 'install' | 'update' | 'configure';

export type SkillMutationReceipt = {
  operationId: string;
  skillId: SkillId;
  action: SkillMutationAction;
  installedVersion: string;
  backupDir: string | null;
  rolledBack: boolean;
  restartRequired: boolean;
  warnings: string[];
};

export type BackupScope = 'full' | 'runtime' | 'runtimeState' | 'sessions' | 'stateOnly';

export type BackupReceiptSummary = {
  backupDir: string;
  sourceRoot: string;
  reason: string;
  createdAtMs: number;
  scope: BackupScope;
  trackedDatabaseCount: number;
  completeSessions: boolean;
};

export type CreateFullBackupReceipt = {
  operationId: string;
  backups: BackupReceiptSummary[];
  warnings: string[];
};

export type BackupDeleteReceipt = {
  operationId: string;
  backupDir: string;
  reclaimedBytes: number;
  warnings: string[];
};

export type CheckpointCleanupSummary = {
  attemptedCount: number;
  failedCount: number;
  reclaimedCount: number;
  reclaimedBytes: number;
  retainedCount: number;
  warnings: string[];
};

export type CheckpointCleanupReceipt = CheckpointCleanupSummary & {
  operationId: string;
};

export type CheckpointStorageStatus = {
  totalCount: number;
  totalBytes: number;
  reclaimableCount: number;
  reclaimableBytes: number;
  retainedCount: number;
  warnings: string[];
  lastCleanup: CheckpointCleanupReceipt | null;
};

export type BackupSummary = {
  backupDir: string;
  sourceRoot: string;
  reason: string;
  createdAtMs: number;
  fileCount: number;
  totalBytes: number;
  verified: boolean;
  completeSessions: boolean;
};

export type RestoreResult = {
  operationId?: string;
  backupDir: string;
  targetRoot: string;
  restoredFiles: number;
  verified: boolean;
  rolledBack?: boolean;
  warnings?: string[];
  safetyBackup?: BackupReceiptSummary;
};

export type OperationAction =
  | 'importAccount'
  | 'saveRelay'
  | 'verifyRelay'
  | 'switchRuntime'
  | 'syncSessions'
  | 'deleteSessions'
  | 'restoreVisibility'
  | 'restoreBackup'
  | 'createBackup'
  | 'deleteBackup'
  | 'cleanupCheckpoints'
  | 'installSkill'
  | 'configureSkill';

export type OperationStatus = 'succeeded' | 'failed' | 'rolledBack' | 'rollbackFailed';
export type OperationPhase = 'preflight' | 'backup' | 'apply' | 'verify' | 'complete' | 'rollback';

export type OperationRecord = {
  operationId: string;
  action: OperationAction;
  status: OperationStatus;
  phase: OperationPhase;
  startedAtMs: number;
  completedAtMs: number;
  backupDirs: string[];
  counts: Record<string, number>;
};

export type SessionSyncResult = {
  operationId?: string;
  backups?: BackupReceiptSummary[];
  insertedThreads: number;
  copiedSessionFiles: number;
  duplicateThreads: number;
  skippedMissingSessionFiles: number;
  skippedArchivedThreads: number;
  mergedSessionIndexEntries: number;
  rolledBack?: boolean;
  warnings?: string[];
  checkpointCleanup?: CheckpointCleanupSummary;
};

export type RuntimeSwitchResult = {
  operationId: string;
  changed: boolean;
  runtime: RuntimeMetadata;
  backups: BackupReceiptSummary[];
  toShared: SessionSyncResult;
  fromShared: SessionSyncResult;
  rolledBack: boolean;
  warnings?: string[];
  checkpointCleanup?: CheckpointCleanupSummary;
};

export type RuntimeSwitchPhase =
  | 'planningSessions'
  | 'detectingApp'
  | 'closingApp'
  | 'verifyingRelay'
  | 'backingUpCurrent'
  | 'backingUpShared'
  | 'syncingToShared'
  | 'applyingRuntime'
  | 'syncingToCurrent'
  | 'verifying'
  | 'rollingBack'
  | 'cleaningCheckpoints'
  | 'complete'
  | 'failed';

export type RuntimeSwitchProgress = {
  phase: RuntimeSwitchPhase;
  timestampMs: number;
  message?: string | null;
  outcome?: 'failedBeforeWrite' | 'rolledBack' | 'rollbackFailed' | null;
};

export type SyncDryRun = {
  sourceThreads: number;
  targetThreads: number;
  newThreads: number;
  duplicateThreads: number;
};

export type AllSessionsDryRun = {
  toShared: SyncDryRun;
  toCurrent: SyncDryRun;
};
