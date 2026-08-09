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

export type DiagnosticStatus = {
  available: boolean;
  eventCount: number;
  totalBytes: number;
  retentionDays: number;
  maxBytes: number;
  oldestEventAtMs: number | null;
  newestEventAtMs: number | null;
  warnings: string[];
};

export type DiagnosticExportReceipt = {
  exportId: string;
  path: string;
  filename: string;
  bytes: number;
  sha256: string;
  eventCount: number;
  selection: DiagnosticExportSelection;
  warnings: string[];
};

export type DiagnosticExportSelection = {
  mode: 'operation' | 'retainedWindow';
  operationId?: string;
  fromTimestampMs: number;
  throughTimestampMs: number;
};

export type DiagnosticExportFailure = {
  kind: 'preparation' | 'destination';
  message: string;
  retryId?: string;
};

export type DiagnosticExportTarget = 'downloads' | 'diagnosticDirectory';

export type FrontendDiagnosticInput = {
  level: 'error';
  component: 'frontend';
  eventKind: 'unhandledError' | 'unhandledRejection';
  errorCode: 'frontend.unhandled_error' | 'frontend.unhandled_rejection';
  safeMessage: string;
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
export type RelaySwitchPreference = 'validate' | 'direct';

export type RuntimeMetadata = {
  id: string;
  name: string;
  kind: RuntimeKind;
  baseUrl: string | null;
  model: string | null;
  createdAtMs: number;
  lastUsedAtMs: number | null;
  lastVerifiedAtMs: number | null;
  relaySwitchPreference?: RelaySwitchPreference | null;
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
  | 'incrementalSync'
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
  persistentSessionBytesAdded: number;
  persistentSessionBytesReclaimed: number;
  rolledBack?: boolean;
  warnings?: string[];
  checkpointCleanup?: CheckpointCleanupSummary;
  chatgptLaunch: ChatGptLaunchResult;
};

export type MobileContinuityItemStatus =
  | 'queued'
  | 'publishing'
  | 'remotePublished'
  | 'partial'
  | 'conflict'
  | 'retrying'
  | 'needsManual'
  | 'paused';

export type MobileContinuityItem = {
  threadId: string;
  status: MobileContinuityItemStatus;
  attempts: number;
  nextRetryAtMs: number | null;
  updatedAtMs: number;
  failureCategory: string | null;
  sourceFingerprint: {
    size: number;
    modifiedAtMs: number;
    sha256: string;
  } | null;
};

export type MobileContinuityStatus = {
  enabled: boolean;
  noticePending: boolean;
  initializedAtMs: number;
  queued: number;
  publishing: number;
  remotePublished: number;
  partial: number;
  conflict: number;
  needsManual: number;
  items: MobileContinuityItem[];
};

export type IncrementalSessionSyncStatus =
  | 'skipped'
  | 'unchanged'
  | 'applied'
  | 'needsFullSync'
  | 'deferred'
  | 'failed';

export type IncrementalSessionSyncReceipt = {
  status: IncrementalSessionSyncStatus;
  detectedThreads: number;
  syncedThreads: number;
  projectedBytes: number;
  durationMs: number;
  requiresFullSync: boolean;
};

export type RuntimeSwitchResult = {
  operationId: string;
  changed: boolean;
  runtime: RuntimeMetadata;
  warnings?: string[];
  incrementalSessionSync: IncrementalSessionSyncReceipt;
  relayValidation: 'notApplicable' | 'verified' | 'skipped';
  chatProcessStateRepaired: boolean;
  chatgptLaunch: ChatGptLaunchResult;
};

export type ChatGptLaunchStatus =
  | 'launched'
  | 'alreadyRunning'
  | 'failed'
  | 'blocked'
  | 'notRequested';

export type ChatGptLaunchResult = {
  status: ChatGptLaunchStatus;
  message: string | null;
};

export type RuntimeSwitchPhase =
  | 'loadingRuntime'
  | 'validatingOfficialAuth'
  | 'verifyingRelay'
  | 'detectingApp'
  | 'closingApp'
  | 'preparingRuntime'
  | 'repairingAppState'
  | 'applyingRuntime'
  | 'verifying'
  | 'recordingResult'
  | 'syncingIncrementalSessions'
  | 'rollingBack'
  | 'launchingApp'
  | 'complete'
  | 'failed';

export type RuntimeSwitchProgress = {
  phase: RuntimeSwitchPhase;
  timestampMs: number;
  operationId?: string | null;
  message?: string | null;
  outcome?: 'failedBeforeWrite' | 'rolledBack' | 'rollbackFailed' | null;
};

export type AppExitRequestResult = {
  scheduled: boolean;
};

export type SessionSyncPhase =
  | 'preparing'
  | 'closingApp'
  | 'backingUp'
  | 'reconciling'
  | 'recordingResult'
  | 'launchingApp'
  | 'complete'
  | 'failed';

export type SessionSyncProgress = {
  phase: SessionSyncPhase;
  timestampMs: number;
  operationId?: string | null;
  message?: string | null;
};
