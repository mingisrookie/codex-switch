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

export type StorageScanStatus =
  | 'noSessions'
  | 'canonicalReady'
  | 'migrationAvailable'
  | 'reviewRequired';

export type ShadowScanIssueCode =
  | 'databaseDiscoveryFailed'
  | 'databaseSnapshotFailed'
  | 'databaseRowMissingRolloutPath'
  | 'sessionDiscoveryFailed'
  | 'sessionParseFailed'
  | 'invalidProviderMarker'
  | 'missingRuntimeReference'
  | 'mismatchedRuntimeReference'
  | 'divergentSession'
  | 'onlineSnapshotNotAtomic'
  | 'reportPersistenceFailed'
  | 'hashCacheInvalid'
  | 'hashCachePersistenceFailed'
  | 'turnProvenanceInvalid'
  | 'storageStateInvalid';

export type RelationCounts = {
  equal: number;
  equalExceptProvider: number;
  prefix: number;
  divergent: number;
  unknown: number;
};

export type ShadowScanSummary = {
  schemaVersion: number;
  onlineScanOnly: boolean;
  nonAtomicAcrossDatabases: boolean;
  logicalSessionCount: number;
  canonicalCandidateCount: number;
  duplicatedSessionCount: number;
  conflictSessionCount: number;
  highConfidenceCopyCount: number;
  sessionFileCount: number;
  sessionBytes: number;
  potentialReclaimBytes: number;
  markerFileCount: number;
  runtimeDatabaseCount: number;
  backupDatabaseCount: number;
  runtimeReferenceCount: number;
  missingRuntimeReferenceCount: number;
  mismatchedRuntimeReferenceCount: number;
  cacheHitCount: number;
  cacheMissCount: number;
  stableFileCount: number;
  turnContextCount: number;
  resolvedTurnProvenanceCount: number;
  historicalUnknownTurnCount: number;
  incompleteTurnProvenanceCount: number;
  relationCounts: RelationCounts;
};

export type ShadowScanReport = {
  schemaVersion: number;
  scanId: string;
  generatedAtMs: number;
  status: StorageScanStatus;
  migrationRequired: boolean;
  deletionEnabled: boolean;
  summary: ShadowScanSummary;
  issues: Array<{ code: ShadowScanIssueCode; count: number }>;
};

export type SessionStorageControlState = {
  schemaVersion: number;
  canonicalReady: boolean;
  migrationOperationId?: string;
  migrationPreparedAtMs?: number;
  automaticCleanupEnabled: boolean;
  onlineDeletionEnabled: boolean;
  reclaimedBytes: number;
};

export type SessionStorageInvestigationReceipt = {
  taskId: string;
  issueCount: number;
  databaseCount: number;
  displayPath: string;
  taskSha256: string;
};

export type SessionRelation =
  | 'equal'
  | 'equalExceptProvider'
  | 'leftPrefix'
  | 'rightPrefix'
  | 'divergent'
  | 'unknown';

export type SessionDatabaseRole =
  | 'canonicalAccount'
  | 'accountView'
  | 'relay'
  | 'shared'
  | 'legacyOrRelocated'
  | 'backup'
  | 'recoveryPackage'
  | 'downgradeExport'
  | 'unknownRuntime';

export type SessionMarkerStatus = 'absent' | 'valid' | 'invalid';

export type FileOrigin =
  | 'canonicalHome'
  | 'shared'
  | 'referencedExternal'
  | 'backupInventory'
  | 'conflictRecycle'
  | 'recoveryPackage'
  | 'downgradeExport'
  | 'temporaryAdapter'
  | 'unknown';

export type MigrationSafetyBlocker =
  | 'inventoryChanged'
  | 'databaseDiscoveryFailed'
  | 'databaseSnapshotFailed'
  | 'sessionDiscoveryFailed'
  | 'backupDestinationUnsafe'
  | 'insufficientBackupSpace'
  | 'canonicalTargetCollision';

export type MigrationSessionAction =
  | 'keepCanonical'
  | 'copyToCanonical'
  | 'replaceCanonicalWithExtension'
  | 'conflict';

export type MigrationDuplicatePlan = {
  path: string;
  bytes: number;
  sha256: string;
  relationToRetained: SessionRelation;
  markerStatus: SessionMarkerStatus;
};

export type MigrationSessionPlan = {
  threadId: string;
  action: MigrationSessionAction;
  retainedPath: string;
  canonicalPath: string;
  retainedBytes: number;
  retainedSha256: string;
  retainedMessageCount: number;
  lastValidMessageAt?: string;
  duplicates: MigrationDuplicatePlan[];
};

export type MigrationConflictPlan = {
  threadId: string;
  currentPath: string;
  candidatePath: string;
  canonicalPath: string;
  currentSha256?: string;
  candidateSha256: string;
  currentOrigin: FileOrigin;
  candidateOrigin: FileOrigin;
  currentMarkerStatus: SessionMarkerStatus;
  candidateMarkerStatus: SessionMarkerStatus;
  currentMessageCount: number;
  candidateMessageCount: number;
  currentLastMessageAt?: string;
  candidateLastMessageAt?: string;
  currentProvider?: string | null;
  candidateProvider?: string | null;
  relation: SessionRelation;
  defaultOverwrite: boolean;
};

export type MigrationDatabasePlan = {
  databaseId: string;
  path: string;
  role: SessionDatabaseRole;
  referenceCount: number;
};

export type CanonicalMigrationPlan = {
  schemaVersion: number;
  operationId: string;
  generatedAtMs: number;
  canonicalRoot: string;
  inventoryFingerprint: string;
  sessions: MigrationSessionPlan[];
  conflicts: MigrationConflictPlan[];
  databases: MigrationDatabasePlan[];
  unclassifiedFileCount: number;
  invalidMarkerCount: number;
  missingRuntimeReferenceCount: number;
  mismatchedRuntimeReferenceCount: number;
};

export type MigrationPreflightReport = {
  schemaVersion: number;
  operationId: string;
  generatedAtMs: number;
  canonicalSessionCount: number;
  sessionFileCount: number;
  providerCopyCount: number;
  conflictCount: number;
  anomalyCount: number;
  estimatedReclaimBytes: number;
  backupSourceBytes: number;
  requiredBackupBytes: number;
  availableBackupBytes: number;
  backupDestination: string;
  blockers: MigrationSafetyBlocker[];
  readyForBackup: boolean;
  plan: CanonicalMigrationPlan;
};

export type MigrationBackupStatus =
  | 'integrityVerified'
  | 'isolatedRestoreVerified'
  | 'runtimeVerified';

export type MigrationBackupEntryKind =
  | 'session'
  | 'sessionIndex'
  | 'database'
  | 'storageMetadata';

export type MigrationBackupEntry = {
  sourcePath: string;
  payloadRelativePath: string;
  kind: MigrationBackupEntryKind;
  bytes: number;
  sha256: string;
  logicalThreadId?: string;
};

export type MigrationBackupManifest = {
  schemaVersion: number;
  operationId: string;
  createdAtMs: number;
  expiresAtMs: number;
  backupDir: string;
  status: MigrationBackupStatus;
  entries: MigrationBackupEntry[];
  isolatedRestoreVerifiedAtMs?: number;
  runtimeVerification?: {
    expectedSessionCount: number;
    listedSessionCount: number;
    resumedSessionCount: number;
    toolSessionCount: number;
    toolRoundTripVerified: boolean;
    verifiedAtMs: number;
  };
};

export type MigrationPreparationReceipt = {
  operationId: string;
  preparedSessionCount: number;
  preparedDatabaseCount: number;
  conflictCount: number;
  preparedBytes: number;
};

export type MigrationCancellationReceipt = {
  operationId: string;
  backupRetained: boolean;
  stagingDiscarded: boolean;
};

export type MigrationApplyReceipt = {
  operationId: string;
  canonicalCreatedCount: number;
  canonicalReplacedCount: number;
  databaseViewCount: number;
  conflictCount: number;
  validated: boolean;
  runtimeVerification?: {
    expectedSessionCount: number;
    listedSessionCount: number;
    resumedSessionCount: number;
    toolSessionCount: number;
    toolRoundTripVerified: boolean;
    verifiedAtMs: number;
  };
};

export type OfflineGcReceipt = {
  operationId: string;
  candidateCount: number;
  deletedCount: number;
  reclaimedBytes: number;
  validated: boolean;
};

export type DowngradeCompatibilityBand = 'a' | 'b' | 'c';

export type DowngradeTargetContract = {
  version: string;
  band: DowngradeCompatibilityBand;
  runtimeBundleRequired: boolean;
  incrementalIndexRequired: boolean;
  relaySessionViewSupported: boolean;
  mobileContinuityRequired: boolean;
};

export type DowngradeExportReceipt = {
  operationId: string;
  target: DowngradeTargetContract;
  packageDir: string;
  logicalSessionCount: number;
  sessionFileCount: number;
  conflictBranchCount: number;
  recoveryPayloadCount: number;
  packageBytes: number;
  containsCredentials: boolean;
  structurallyVerified: boolean;
  nativeRuntimeVerified: boolean;
  targetRuntimeVerificationRequired: boolean;
};

export type RestoreImportReceipt = {
  operationId: string;
  packageOperationId: string;
  targetVersion: string;
  packageDir: string;
  scannedSessionCount: number;
  unchangedSessionCount: number;
  currentAheadSessionCount: number;
  importedNewSessionCount: number;
  importedExtensionCount: number;
  conflictCount: number;
  unclassifiedRecoveryCount: number;
  unclassifiedRecoveryBytes: number;
  unclassifiedRecoveryPaths: string[];
  anomalyCount: number;
  databaseViewCount: number;
  importedBytes: number;
  recoveryExpiresAtMs: number;
  validated: boolean;
  runtimeVerification?: {
    expectedSessionCount: number;
    listedSessionCount: number;
    resumedSessionCount: number;
    toolSessionCount: number;
    toolRoundTripVerified: boolean;
    verifiedAtMs: number;
  };
};

export type PendingRecoveryRelation =
  | 'missingFromCanonical'
  | 'extendsCanonical'
  | 'divergent'
  | 'unknown';

export type PendingRecoveryStatus = 'pending' | 'restored' | 'deferred';

export type PendingRecoverySummary = {
  entryId: string;
  threadId: string;
  relation: PendingRecoveryRelation;
  status: PendingRecoveryStatus;
  sourceBackupId: string;
  sourceBackupCreatedAtMs: number;
  candidateMessageCount: number;
  currentMessageCount: number;
  candidateAddedMessageCount: number;
  currentAddedMessageCount: number;
  candidateLastMessageAt?: string;
  currentLastMessageAt?: string;
  candidateProvider?: string;
  currentProvider?: string;
  payloadBytes: number;
  expiresAtMs: number;
  restoreAllowed: boolean;
};

export type PendingRecoveryList = {
  migrationOperationId: string;
  entries: PendingRecoverySummary[];
  expiredPackageCount: number;
  invalidPackageCount: number;
};

export type LegacyBackupReconciliationReceipt = {
  operationId: string;
  migrationOperationId: string;
  scannedBackupCount: number;
  deletedBackupCount: number;
  retainedBackupCount: number;
  unreadableBackupCount: number;
  pendingRecoveryCount: number;
  conflictCount: number;
  reclaimedBytes: number;
  validated: boolean;
};

export type ConflictVersion = 'current' | 'candidate';

export type SessionConflictSummary = {
  conflictId: string;
  deferred: boolean;
  currentMessageCount: number;
  candidateMessageCount: number;
  currentAddedMessageCount: number;
  candidateAddedMessageCount: number;
  currentLastMessageAt?: string;
  candidateLastMessageAt?: string;
  currentProvider?: string;
  candidateProvider?: string;
  currentOrigin: FileOrigin;
  candidateOrigin: FileOrigin;
  relation: SessionRelation;
  newerVersion?: ConflictVersion;
  defaultOverwrite: boolean;
};

export type SessionConflictList = {
  migrationOperationId: string;
  conflicts: SessionConflictSummary[];
};

export type ConflictResolutionAction = 'defer' | 'useNewer';

export type ConflictResolutionReceipt = {
  operationId?: string;
  migrationOperationId: string;
  conflictId: string;
  status: 'deferred' | 'resolved';
  chosenVersion?: ConflictVersion;
  canonicalUpdated: boolean;
  databaseViewCount: number;
  recoveryExpiresAtMs?: number;
  runtimeVerification?: {
    expectedSessionCount: number;
    listedSessionCount: number;
    resumedSessionCount: number;
    toolSessionCount: number;
    toolRoundTripVerified: boolean;
    verifiedAtMs: number;
  };
  validated: boolean;
};

export type DomainState<T> =
  | { status: 'loading' }
  | { status: 'ready'; data: T }
  | { status: 'error'; error: string };

export type DashboardData = {
  codexHome: DomainState<CodexHomeStatus>;
  sessions: DomainState<SessionInventory>;
  managedSessions: DomainState<ManagedSessionInventory>;
  sessionStorage: DomainState<ShadowScanReport | null>;
  runtimes: DomainState<RuntimeMetadata[]>;
  runtimeStatus: DomainState<RuntimeStatus>;
  backups: DomainState<BackupSummary[]>;
  backupStorage: DomainState<CheckpointStorageStatus>;
  operations: DomainState<OperationRecord[]>;
};

export type RuntimeDashboardData = Pick<
  DashboardData,
  'codexHome' | 'sessionStorage' | 'runtimes' | 'runtimeStatus' | 'operations'
>;

export type SessionDashboardData = Pick<
  DashboardData,
  'sessions' | 'managedSessions' | 'sessionStorage'
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
  routeProvenance: {
    status: 'pending' | 'recorded' | 'unchanged' | 'failed';
    message?: string | null;
  };
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
