import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  ArchiveRestore,
  Check,
  CircleAlert,
  Database,
  Download,
  FileSearch,
  FolderOpen,
  HardDrive,
  LoaderCircle,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  Trash2,
} from 'lucide-react';
import {
  applySessionStorageMigration,
  cancelSessionStorageMigration,
  createSessionStorageInvestigationTask,
  createSessionStorageMigrationBackup,
  exportSessionStorageDowngrade,
  getSessionStorageControlState,
  getSessionStorageStatus,
  importSessionStorageDowngrade,
  reconcileSessionStorageLegacyBackups,
  listSessionStoragePendingRecovery,
  deferSessionStoragePendingRecovery,
  restoreSessionStoragePendingRecovery,
  listSessionStorageConflicts,
  openSessionStorageInvestigationTask,
  preflightSessionStorageMigration,
  prepareSessionStorageMigration,
  resolveSessionStorageConflict,
  runSessionStorageOfflineGc,
  scanSessionStorage,
  setSessionStorageAutomaticCleanup,
  verifySessionStorageMigrationBackup,
} from './api';
import type {
  ConflictResolutionAction,
  ConflictResolutionReceipt,
  DowngradeExportReceipt,
  MigrationApplyReceipt,
  MigrationBackupManifest,
  MigrationCancellationReceipt,
  MigrationPreflightReport,
  MigrationPreparationReceipt,
  LegacyBackupReconciliationReceipt,
  OfflineGcReceipt,
  PendingRecoveryList,
  PendingRecoverySummary,
  RestoreImportReceipt,
  SessionConflictList,
  SessionConflictSummary,
  SessionStorageControlState,
  SessionStorageInvestigationReceipt,
  ShadowScanIssueCode,
  ShadowScanReport,
} from './types';

type StorageActionReceipt =
  | MigrationCancellationReceipt
  | MigrationApplyReceipt
  | MigrationPreparationReceipt
  | OfflineGcReceipt
  | DowngradeExportReceipt
  | RestoreImportReceipt
  | LegacyBackupReconciliationReceipt
  | ConflictResolutionReceipt;

export type SessionStorageManagementDependencies = {
  getControlState: typeof getSessionStorageControlState;
  getStatus: typeof getSessionStorageStatus;
  setAutomaticCleanup: typeof setSessionStorageAutomaticCleanup;
  scan: typeof scanSessionStorage;
  createInvestigationTask: typeof createSessionStorageInvestigationTask;
  openInvestigationTask: typeof openSessionStorageInvestigationTask;
  preflight: typeof preflightSessionStorageMigration;
  createBackup: typeof createSessionStorageMigrationBackup;
  verifyBackup: typeof verifySessionStorageMigrationBackup;
  prepareMigration: typeof prepareSessionStorageMigration;
  cancelMigration: typeof cancelSessionStorageMigration;
  applyMigration: typeof applySessionStorageMigration;
  runOfflineGc: typeof runSessionStorageOfflineGc;
  listConflicts: typeof listSessionStorageConflicts;
  resolveConflict: typeof resolveSessionStorageConflict;
  exportDowngrade: typeof exportSessionStorageDowngrade;
  importDowngrade: typeof importSessionStorageDowngrade;
  reconcileLegacyBackups: typeof reconcileSessionStorageLegacyBackups;
  listPendingRecovery: typeof listSessionStoragePendingRecovery;
  deferPendingRecovery: typeof deferSessionStoragePendingRecovery;
  restorePendingRecovery: typeof restoreSessionStoragePendingRecovery;
};

const defaultDependencies: SessionStorageManagementDependencies = {
  getControlState: getSessionStorageControlState,
  getStatus: getSessionStorageStatus,
  setAutomaticCleanup: setSessionStorageAutomaticCleanup,
  scan: scanSessionStorage,
  createInvestigationTask: createSessionStorageInvestigationTask,
  openInvestigationTask: openSessionStorageInvestigationTask,
  preflight: preflightSessionStorageMigration,
  createBackup: createSessionStorageMigrationBackup,
  verifyBackup: verifySessionStorageMigrationBackup,
  prepareMigration: prepareSessionStorageMigration,
  cancelMigration: cancelSessionStorageMigration,
  applyMigration: applySessionStorageMigration,
  runOfflineGc: runSessionStorageOfflineGc,
  listConflicts: listSessionStorageConflicts,
  resolveConflict: resolveSessionStorageConflict,
  exportDowngrade: exportSessionStorageDowngrade,
  importDowngrade: importSessionStorageDowngrade,
  reconcileLegacyBackups: reconcileSessionStorageLegacyBackups,
  listPendingRecovery: listSessionStoragePendingRecovery,
  deferPendingRecovery: deferSessionStoragePendingRecovery,
  restorePendingRecovery: restoreSessionStoragePendingRecovery,
};

type SessionStorageManagementPageProps = {
  active: boolean;
  initialReport: ShadowScanReport | null;
  onReportChange?: (report: ShadowScanReport) => void;
  onBusyChange?: (label: string | null) => void;
  dependencies?: SessionStorageManagementDependencies;
};

const versionOptions = [
  'v0.2.0',
  'v0.2.1',
  'v0.2.2',
  'v0.2.3',
  'v0.2.4',
  'v0.2.5',
  'v0.2.6',
  'v0.2.7',
];

const numberFormat = new Intl.NumberFormat('zh-CN');
const shadowJoinPollIntervalMs = 250;
const shadowJoinTimeoutMs = 30_000;
const shadowScanAlreadyRunningMessage = 'a session storage shadow scan is already running';

const investigationIssueCodes = new Set<ShadowScanIssueCode>([
  'databaseDiscoveryFailed',
  'databaseSnapshotFailed',
  'databaseRowMissingRolloutPath',
  'sessionDiscoveryFailed',
  'sessionParseFailed',
  'missingRuntimeReference',
  'mismatchedRuntimeReference',
  'turnProvenanceInvalid',
  'storageStateInvalid',
]);

export function SessionStorageManagementPage({
  active,
  initialReport,
  onReportChange = () => undefined,
  onBusyChange = () => undefined,
  dependencies = defaultDependencies,
}: SessionStorageManagementPageProps) {
  const [report, setReport] = useState(initialReport);
  const [control, setControl] = useState<SessionStorageControlState | null>(null);
  const [controlLoading, setControlLoading] = useState(true);
  const [preflight, setPreflight] = useState<MigrationPreflightReport | null>(null);
  const [backup, setBackup] = useState<MigrationBackupManifest | null>(null);
  const [preparation, setPreparation] = useState<MigrationPreparationReceipt | null>(null);
  const [backupDestination, setBackupDestination] = useState('');
  const [writersClosed, setWritersClosed] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [lastReceipt, setLastReceipt] = useState<StorageActionReceipt | null>(null);
  const [conflicts, setConflicts] = useState<SessionConflictList | null>(null);
  const [conflictsLoading, setConflictsLoading] = useState(false);
  const [downgradeVersion, setDowngradeVersion] = useState('v0.2.7');
  const [downgradeDestination, setDowngradeDestination] = useState('');
  const [downgradePackage, setDowngradePackage] = useState('');
  const [pendingRecovery, setPendingRecovery] = useState<PendingRecoveryList | null>(null);
  const [pendingRecoveryLoading, setPendingRecoveryLoading] = useState(false);
  const [investigationReceipt, setInvestigationReceipt] = useState<SessionStorageInvestigationReceipt | null>(null);
  const [legacyReconciliationConfirmOpen, setLegacyReconciliationConfirmOpen] = useState(false);
  const mounted = useRef(false);
  const legacyReconciliationTriggerRef = useRef<HTMLButtonElement | null>(null);
  const legacyReconciliationConfirmationRef = useRef<HTMLHeadingElement | null>(null);
  const legacyReconciliationRestoreFocus = useRef(false);
  const shadowJoinRequestId = useRef(0);
  const shadowJoinPollTimer = useRef<number | null>(null);
  const shadowJoinDeadlineTimer = useRef<number | null>(null);

  useEffect(() => setReport(initialReport), [initialReport]);

  useEffect(() => {
    if (active) return;
    legacyReconciliationRestoreFocus.current = false;
    setLegacyReconciliationConfirmOpen(false);
  }, [active]);

  useEffect(() => {
    if (legacyReconciliationConfirmOpen) {
      legacyReconciliationConfirmationRef.current?.focus();
      return;
    }
    if (!busy && legacyReconciliationRestoreFocus.current) {
      legacyReconciliationRestoreFocus.current = false;
      legacyReconciliationTriggerRef.current?.focus();
    }
  }, [busy, legacyReconciliationConfirmOpen]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      shadowJoinRequestId.current += 1;
      if (shadowJoinPollTimer.current !== null) {
        window.clearTimeout(shadowJoinPollTimer.current);
        shadowJoinPollTimer.current = null;
      }
      if (shadowJoinDeadlineTimer.current !== null) {
        window.clearTimeout(shadowJoinDeadlineTimer.current);
        shadowJoinDeadlineTimer.current = null;
      }
    };
  }, []);

  useEffect(() => {
    if (!active) return;
    let cancelled = false;
    setControlLoading(true);
    dependencies.getControlState()
      .then((next) => {
        if (!cancelled) setControl(next);
      })
      .catch((reason: unknown) => {
        if (!cancelled) setError(errorMessage(reason));
      })
      .finally(() => {
        if (!cancelled) setControlLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [active, dependencies]);

  const migrationOperationId = preflight?.operationId ?? control?.migrationOperationId;
  const migrationReady = Boolean(control?.canonicalReady && migrationOperationId);
  const backupVerified = backup?.status === 'runtimeVerified';
  const canApply = Boolean(preparation && backupVerified && writersClosed && preflight);
  const investigationRequired = Boolean(report?.issues.some(
    (issue) => issue.count > 0 && investigationIssueCodes.has(issue.code),
  ));
  const unclassifiedRestoreReceipt = isRestoreImportReceipt(lastReceipt)
    && lastReceipt.unclassifiedRecoveryCount > 0
    ? lastReceipt
    : null;
  const steps = useMemo(() => [
    { label: '只读预检', done: Boolean(preflight) },
    { label: '完整备份', done: Boolean(backup) },
    { label: '真实恢复验证', done: backupVerified },
    { label: '生成原子计划', done: Boolean(preparation) },
    { label: '提交并验证', done: Boolean(control?.canonicalReady) },
  ], [backup, backupVerified, control?.canonicalReady, preflight, preparation]);

  useEffect(() => {
    if (!active) return;
    if (!migrationReady || !migrationOperationId) {
      setConflicts(null);
      return;
    }
    let cancelled = false;
    setConflictsLoading(true);
    dependencies.listConflicts(migrationOperationId)
      .then((next) => {
        if (!cancelled) setConflicts(next);
      })
      .catch((reason: unknown) => {
        if (!cancelled) setError(errorMessage(reason));
      })
      .finally(() => {
        if (!cancelled) setConflictsLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [active, dependencies, migrationOperationId, migrationReady]);

  useEffect(() => {
    if (!active) return;
    if (!migrationReady || !migrationOperationId) {
      setPendingRecovery(null);
      return;
    }
    let cancelled = false;
    setPendingRecoveryLoading(true);
    dependencies.listPendingRecovery(migrationOperationId)
      .then((next) => {
        if (!cancelled) setPendingRecovery(next);
      })
      .catch((reason: unknown) => {
        if (!cancelled) setError(errorMessage(reason));
      })
      .finally(() => {
        if (!cancelled) setPendingRecoveryLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [active, dependencies, migrationOperationId, migrationReady]);

  async function runAction<T>(
    label: string,
    action: () => Promise<T>,
    onSuccess: (value: T) => void | Promise<void>,
  ) {
    if (busy) return;
    setBusy(label);
    onBusyChange(label);
    setError(null);
    setNotice(null);
    try {
      const value = await action();
      await onSuccess(value);
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(null);
      onBusyChange(null);
    }
  }

  function clearShadowJoinTimers() {
    if (shadowJoinPollTimer.current !== null) {
      window.clearTimeout(shadowJoinPollTimer.current);
      shadowJoinPollTimer.current = null;
    }
    if (shadowJoinDeadlineTimer.current !== null) {
      window.clearTimeout(shadowJoinDeadlineTimer.current);
      shadowJoinDeadlineTimer.current = null;
    }
  }

  function cancelShadowJoin() {
    shadowJoinRequestId.current += 1;
    clearShadowJoinTimers();
  }

  function publishShadowReport(nextReport: ShadowScanReport) {
    if (!mounted.current) return;
    setReport(nextReport);
    onReportChange(nextReport);
  }

  function joinBackgroundShadow(previousScanId: string | null) {
    cancelShadowJoin();
    const requestId = shadowJoinRequestId.current;

    shadowJoinDeadlineTimer.current = window.setTimeout(() => {
      if (shadowJoinRequestId.current !== requestId) return;
      shadowJoinRequestId.current += 1;
      clearShadowJoinTimers();
    }, shadowJoinTimeoutMs);

    const poll = async () => {
      let nextReport: ShadowScanReport | null = null;
      try {
        nextReport = await dependencies.getStatus();
      } catch {
        // A cached-status read is best effort; keep joining until the bounded deadline.
      }
      if (!mounted.current || shadowJoinRequestId.current !== requestId) return;
      if (nextReport && nextReport.scanId !== previousScanId) {
        clearShadowJoinTimers();
        publishShadowReport(nextReport);
        return;
      }
      shadowJoinPollTimer.current = window.setTimeout(() => {
        shadowJoinPollTimer.current = null;
        void poll();
      }, shadowJoinPollIntervalMs);
    };

    void poll();
  }

  function refreshControlAndJoinShadow(previousScanId: string | null) {
    void dependencies.getControlState()
      .then((nextControl) => {
        if (mounted.current) setControl(nextControl);
      })
      .catch((reason: unknown) => {
        if (mounted.current) {
          setError(`操作已完成，但控制状态刷新失败：${errorMessage(reason)}`);
        }
      });
    joinBackgroundShadow(previousScanId);
  }

  function handleScan() {
    const previousScanId = report?.scanId ?? null;
    void runAction('扫描会话存储', async () => {
      try {
        return await dependencies.scan();
      } catch (reason) {
        if (!isShadowScanAlreadyRunning(reason)) throw reason;
        joinBackgroundShadow(previousScanId);
        return null;
      }
    }, (next) => {
      if (next) {
        cancelShadowJoin();
        publishShadowReport(next);
        setNotice('扫描完成；在线模式未删除任何会话正文。');
      } else {
        setNotice('后台扫描正在运行；已加入本次扫描，最新报告就绪后会自动刷新。');
      }
    });
  }

  function handleCreateInvestigationTask() {
    if (!investigationRequired) return;
    void runAction(
      '生成 Codex 只读排查任务',
      dependencies.createInvestigationTask,
      (next) => {
        setInvestigationReceipt(next as SessionStorageInvestigationReceipt);
        setNotice('本地脱敏排查任务已生成；任务明确要求先只读排查、不修改数据。');
      },
    );
  }

  function handleOpenInvestigationTask() {
    if (!investigationReceipt) return;
    void runAction(
      '打开 Codex 排查任务目录',
      () => dependencies.openInvestigationTask(investigationReceipt.taskId),
      () => setNotice('已打开本地脱敏排查任务目录。'),
    );
  }

  function handlePreflight() {
    const destination = backupDestination.trim();
    if (!destination) {
      setError('请先填写完整备份目录。');
      return;
    }
    void runAction('执行迁移预检', () => dependencies.preflight(destination), (next) => {
      setPreflight(next as MigrationPreflightReport);
      setBackup(null);
      setPreparation(null);
      setLastReceipt(null);
      setNotice('预检完成；尚未修改 canonical 会话或数据库。');
    });
  }

  function handleCreateBackup() {
    if (!preflight) return;
    void runAction('创建完整迁移备份', () => dependencies.createBackup(preflight.operationId), (next) => {
      setBackup(next as MigrationBackupManifest);
      setNotice('完整备份已创建，必须继续通过真实 Codex 隔离恢复验证。');
    });
  }

  function handleVerifyBackup() {
    if (!preflight) return;
    void runAction('验证完整迁移备份', () => dependencies.verifyBackup(preflight.operationId), (next) => {
      setBackup(next as MigrationBackupManifest);
      setNotice('完整备份已通过隔离恢复和真实 Codex 读取验证。');
    });
  }

  function handlePrepareMigration() {
    if (!preflight) return;
    void runAction('生成迁移计划', () => dependencies.prepareMigration(preflight.operationId), (next) => {
      setPreparation(next as MigrationPreparationReceipt);
      setLastReceipt(next as MigrationPreparationReceipt);
      setNotice('原子迁移计划已就绪；提交前仍可取消。');
    });
  }

  function handleCancelMigration() {
    if (!preflight) return;
    void runAction('取消迁移', () => dependencies.cancelMigration(preflight.operationId), (next) => {
      setLastReceipt(next as MigrationCancellationReceipt);
      setPreflight(null);
      setBackup(null);
      setPreparation(null);
      setWritersClosed(false);
      setNotice('迁移已取消；canonical 数据未切换。');
    });
  }

  function handleApplyMigration() {
    if (!preflight || !canApply) return;
    const previousScanId = report?.scanId ?? null;
    void runAction('提交会话存储迁移', () => dependencies.applyMigration(preflight.operationId), (next) => {
      setLastReceipt(next as MigrationApplyReceipt);
      refreshControlAndJoinShadow(previousScanId);
      setNotice('迁移已通过数据校验和真实 Codex 运行时验证；不会自动重启 Codex。');
    });
  }

  function handleAutomaticCleanup(enabled: boolean) {
    const previousScanId = report?.scanId ?? null;
    void runAction('更新自动清理设置', () => dependencies.setAutomaticCleanup(enabled), (next) => {
      setControl(next as SessionStorageControlState);
      if (enabled) joinBackgroundShadow(previousScanId);
      setNotice(enabled
        ? '自动清理已开启；在线仍只扫描，检测到所有 writer 已关闭的安全窗口后会自动执行离线清理。'
        : '自动清理已关闭；仅停止 provider 副本自动 GC，扫描、报告和 7 天隐私/恢复生命周期仍继续，旧 provider 复制逻辑不会恢复。');
    });
  }

  function handleOfflineGc() {
    if (!migrationOperationId || !writersClosed) return;
    const previousScanId = report?.scanId ?? null;
    void runAction('执行离线会话清理', () => dependencies.runOfflineGc(migrationOperationId), (next) => {
      setLastReceipt(next as OfflineGcReceipt);
      refreshControlAndJoinShadow(previousScanId);
      setNotice('离线清理完成；仅删除通过全局零引用与内容关系复核的候选。');
    });
  }

  function handleResolveConflict(conflict: SessionConflictSummary, action: ConflictResolutionAction) {
    if (!migrationOperationId) return;
    const previousScanId = report?.scanId ?? null;
    void runAction(action === 'defer' ? '暂不覆盖冲突' : '切换冲突主版本', () => (
      dependencies.resolveConflict(migrationOperationId, conflict.conflictId, action)
    ), async (next) => {
      setLastReceipt(next as ConflictResolutionReceipt);
      if (action === 'defer') {
        setConflicts(await dependencies.listConflicts(migrationOperationId));
        setNotice('已持久化“暂不覆盖”；保持当前主版本，候选正文未被删除。');
      } else {
        const [nextConflicts, nextPendingRecovery] = await Promise.all([
          dependencies.listConflicts(migrationOperationId),
          dependencies.listPendingRecovery(migrationOperationId),
        ]);
        setConflicts(nextConflicts);
        setPendingRecovery(nextPendingRecovery);
        refreshControlAndJoinShadow(previousScanId);
        setNotice('冲突主版本已原子切换；旧版本进入 7 天冲突回收。');
      }
    });
  }

  function handleExportDowngrade() {
    if (!migrationOperationId || !writersClosed) return;
    const destination = downgradeDestination.trim();
    if (!destination) {
      setError('请填写隔离降级包的目标目录。');
      return;
    }
    void runAction('生成隔离降级包', () => (
      dependencies.exportDowngrade(migrationOperationId, downgradeVersion, destination)
    ), (next) => {
      const receipt = next as DowngradeExportReceipt;
      setLastReceipt(receipt);
      setDowngradePackage(receipt.packageDir);
      setNotice(receipt.nativeRuntimeVerified
        ? '隔离包已完成结构校验和当前原生 Codex 的隔离列表/读取/恢复验证；正式降级前仍须用目标旧版本完成真实列表、恢复和继续验证。该包绑定生成目录，请勿移动；如需换盘请在最终目录重新生成。'
        : '隔离包尚未完成原生 Codex 运行验证，不能用于正式降级。');
    });
  }

  function handleImportDowngrade() {
    if (!migrationOperationId || !writersClosed) return;
    const packageDir = downgradePackage.trim();
    if (!packageDir) {
      setError('请填写使用过的隔离降级包目录。');
      return;
    }
    const previousScanId = report?.scanId ?? null;
    void runAction('导入旧版本新增会话', () => (
      dependencies.importDowngrade(migrationOperationId, packageDir)
    ), async (next) => {
      const receipt = next as RestoreImportReceipt;
      setLastReceipt(receipt);
      setConflicts(await dependencies.listConflicts(migrationOperationId));
      refreshControlAndJoinShadow(previousScanId);
      setNotice(restoreImportNotice(
        '旧版本新增或完整延续会话已导入；分叉默认保持不覆盖。',
        receipt,
      ));
    });
  }

  function closeLegacyReconciliationConfirmation() {
    legacyReconciliationRestoreFocus.current = true;
    setLegacyReconciliationConfirmOpen(false);
  }

  function handleReconcileLegacyBackups() {
    if (!migrationOperationId || !writersClosed || !legacyReconciliationConfirmOpen) return;
    const previousScanId = report?.scanId ?? null;
    void runAction('整理旧版完整备份', () => (
      dependencies.reconcileLegacyBackups(migrationOperationId)
    ), async (next) => {
      setLastReceipt(next as LegacyBackupReconciliationReceipt);
      const [nextPendingRecovery, nextConflicts] = await Promise.all([
        dependencies.listPendingRecovery(migrationOperationId),
        dependencies.listConflicts(migrationOperationId),
      ]);
      setPendingRecovery(nextPendingRecovery);
      setConflicts(nextConflicts);
      refreshControlAndJoinShadow(previousScanId);
      closeLegacyReconciliationConfirmation();
      setNotice('旧备份已逐份验证；独有会话先进入 7 天待恢复区，之后才删除原备份。');
    });
  }

  function handlePendingRecovery(entry: PendingRecoverySummary, action: 'restore' | 'defer') {
    if (!migrationOperationId) return;
    if (action === 'defer') {
      void runAction('暂不恢复旧备份会话', () => (
        dependencies.deferPendingRecovery(migrationOperationId, entry.entryId)
      ), (next) => {
        setPendingRecovery(next as PendingRecoveryList);
        setNotice('已暂不恢复；canonical 正文没有变化。');
      });
      return;
    }
    if (!writersClosed || !entry.restoreAllowed) return;
    const previousScanId = report?.scanId ?? null;
    void runAction('恢复旧备份会话', () => (
      dependencies.restorePendingRecovery(migrationOperationId, entry.entryId)
    ), async (next) => {
      const receipt = next as RestoreImportReceipt;
      setLastReceipt(receipt);
      setPendingRecovery(await dependencies.listPendingRecovery(migrationOperationId));
      refreshControlAndJoinShadow(previousScanId);
      setNotice(restoreImportNotice(
        '待恢复会话已通过原子导入和真实 Codex 运行时验证。',
        receipt,
      ));
    });
  }

  return (
    <section className="storage-management-page" hidden={!active} aria-label="会话存储管理">
      <header className="storage-management-hero">
        <div>
          <p className="eyebrow">CANONICAL STORAGE / CONTROL 03</p>
          <h1>一份正文，所有账号共用视图</h1>
          <p>迁移、冲突、离线清理和降级都在本机执行；列表可见不会上传会话正文。</p>
        </div>
        <div className="storage-hero-actions">
          <button className="ghost-button" onClick={handleScan} disabled={Boolean(busy)}>
            {busy === '扫描会话存储'
              ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
              : <RefreshCw className="button-icon" aria-hidden="true" />}
            重新扫描
          </button>
        </div>
      </header>

      {busy ? <p className="busy-banner" role="status"><LoaderCircle className="spin" aria-hidden="true" />{busy}</p> : null}
      {error ? <p className="error-banner" role="alert"><strong>存储操作：</strong>{error}</p> : null}
      {notice ? <p className="storage-notice" role="status"><Check aria-hidden="true" />{notice}</p> : null}

      <section className="storage-status-grid" aria-label="存储摘要">
        <StorageMetric label="存储状态" value={controlLoading ? '读取中' : control?.canonicalReady ? 'Canonical 已就绪' : '等待迁移'} />
        <StorageMetric label="Canonical 会话" value={formatNumber(report?.summary.canonicalCandidateCount)} />
        <StorageMetric label="安全副本" value={formatNumber(report?.summary.highConfidenceCopyCount)} />
        <StorageMetric label="冲突" value={formatNumber(report?.summary.conflictSessionCount)} tone={report?.summary.conflictSessionCount ? 'warning' : 'normal'} />
        <StorageMetric label="已回收" value={formatBytes(control?.reclaimedBytes ?? 0)} />
        <StorageMetric label="在线删除" value={control?.onlineDeletionEnabled ? '已启用' : '仅扫描'} />
      </section>

      <div className="storage-management-grid">
        {investigationRequired || investigationReceipt ? (
          <section className="storage-workflow-card" aria-label="交给 Codex 排查">
            <CardHeading icon={<FileSearch aria-hidden="true" />} eyebrow="READ-ONLY INVESTIGATION" title="交给 Codex 排查" />
            <p>当前扫描发现迁移范围外的引用或完整性问题。这里只生成本地脱敏任务，不读取正文，也不自动修复。</p>
            <button
              className="ghost-button full"
              onClick={handleCreateInvestigationTask}
              disabled={Boolean(busy || !investigationRequired || investigationReceipt)}
            >
              <FileSearch className="button-icon" aria-hidden="true" />交给 Codex 排查
            </button>
            {investigationReceipt ? (
              <div className="storage-preflight-details">
                <p className="storage-safe-copy"><ShieldCheck aria-hidden="true" />已生成 {numberFormat.format(investigationReceipt.issueCount)} 条问题摘要，覆盖 {numberFormat.format(investigationReceipt.databaseCount)} 个数据库区域。</p>
                <p>任务位置：<code>{investigationReceipt.displayPath}</code></p>
                <button className="ghost-button full" onClick={handleOpenInvestigationTask} disabled={Boolean(busy)}>
                  <FolderOpen className="button-icon" aria-hidden="true" />打开任务目录
                </button>
              </div>
            ) : null}
          </section>
        ) : null}

        <section className="storage-workflow-card storage-migration-card" aria-label="一次性迁移">
          <CardHeading icon={<Database aria-hidden="true" />} eyebrow="ONE-TIME MIGRATION" title="一次性前台迁移" />
          <ol className="storage-step-list">
            {steps.map((step, index) => (
              <li className={step.done ? 'done' : ''} key={step.label}>
                <span>{step.done ? <Check aria-hidden="true" /> : index + 1}</span>{step.label}
              </li>
            ))}
          </ol>
          {control?.canonicalReady ? (
            <p className="storage-safe-copy"><ShieldCheck aria-hidden="true" />已提交的 migration：<code>{control.migrationOperationId}</code></p>
          ) : (
            <>
              <label className="field-label" htmlFor="migration-backup-destination">完整备份目录</label>
              <input
                id="migration-backup-destination"
                value={backupDestination}
                onChange={(event) => setBackupDestination(event.target.value)}
                placeholder="例如 E:\\CodexSwitchBackups"
                disabled={Boolean(busy || preflight)}
              />
              <div className="storage-action-row">
                <button className="primary-button" onClick={handlePreflight} disabled={Boolean(busy || preflight || !backupDestination.trim())}>开始只读预检</button>
                {preflight ? <button className="ghost-button" onClick={handleCancelMigration} disabled={Boolean(busy)}>取消迁移</button> : null}
              </div>
            </>
          )}
          {preflight ? <MigrationPreflightDetails report={preflight} /> : null}
          {preflight && !control?.canonicalReady ? (
            <div className="storage-action-stack">
              <button className="ghost-button" onClick={handleCreateBackup} disabled={Boolean(busy || backup || !preflight.readyForBackup)}><HardDrive className="button-icon" aria-hidden="true" />创建完整备份</button>
              <button className="ghost-button" onClick={handleVerifyBackup} disabled={Boolean(busy || !backup || backupVerified)}><ShieldCheck className="button-icon" aria-hidden="true" />真实恢复验证</button>
              <button className="ghost-button" onClick={handlePrepareMigration} disabled={Boolean(busy || !backupVerified || preparation)}><Database className="button-icon" aria-hidden="true" />生成原子计划</button>
              <label className="storage-check-row">
                <input type="checkbox" checked={writersClosed} onChange={(event) => setWritersClosed(event.target.checked)} disabled={Boolean(busy)} />
                我已关闭 Codex Desktop、CLI 和其他会话写入进程
              </label>
              <button className="warm-button" onClick={handleApplyMigration} disabled={Boolean(busy || !canApply)}>提交迁移并验证</button>
            </div>
          ) : null}
        </section>

        <section className="storage-workflow-card" aria-label="清理设置">
          <CardHeading icon={<Trash2 aria-hidden="true" />} eyebrow="SAFE RECLAIM" title="自动清理与离线 GC" />
          <label className="storage-toggle-row">
            <span><strong>自动清理</strong><small>关闭只停止 provider 副本自动 GC；扫描、报告和 7 天隐私/恢复生命周期仍继续。</small></span>
            <input
              type="checkbox"
              checked={control?.automaticCleanupEnabled ?? false}
              onChange={(event) => handleAutomaticCleanup(event.target.checked)}
              disabled={Boolean(busy || !control)}
            />
          </label>
          <p className="storage-safe-copy"><ShieldCheck aria-hidden="true" />在线阶段固定只扫描；系统会等待所有 writer 关闭后自动执行离线删除，并重新检查全局引用、hash、句柄和写入状态。下方按钮仅用于立即手动触发同一安全流程；该开关不影响 7 天隐私日志与冲突/恢复包生命周期。</p>
          <label className="storage-check-row">
            <input type="checkbox" checked={writersClosed} onChange={(event) => setWritersClosed(event.target.checked)} disabled={Boolean(busy)} />
            所有 Codex 写入进程已关闭
          </label>
          <button className="ghost-button full" onClick={handleOfflineGc} disabled={Boolean(busy || !migrationReady || !writersClosed || !control)}><Trash2 className="button-icon" aria-hidden="true" />执行离线安全清理</button>
        </section>

        <section className="storage-workflow-card storage-conflict-card" aria-label="冲突处理">
          <CardHeading icon={<CircleAlert aria-hidden="true" />} eyebrow="CONFLICT REVIEW" title="冲突处理" />
          {!migrationReady ? <p>迁移完成后可处理真实分叉。</p> : conflictsLoading ? <p>正在读取脱敏差异摘要…</p> : conflicts?.conflicts.length ? (
            <div className="storage-conflict-list">
              {conflicts.conflicts.map((conflict) => (
                <ConflictCard
                  key={conflict.conflictId}
                  conflict={conflict}
                  disabled={Boolean(busy)}
                  onAction={(action) => handleResolveConflict(conflict, action)}
                />
              ))}
            </div>
          ) : <p>没有待处理的会话分叉。</p>}
        </section>

        <section className="storage-workflow-card" aria-label="显式降级导出">
          <CardHeading icon={<Download aria-hidden="true" />} eyebrow="EXPLICIT DOWNGRADE" title="v0.2.x 隔离降级" />
          <p className="storage-warning-copy"><CircleAlert aria-hidden="true" />降级包包含完整会话正文和本地凭据，只能保存在你选择的本地目录。请直接选择最终位置，生成后不要移动；如需换盘请重新生成。</p>
          <label className="field-label" htmlFor="downgrade-version">目标旧版本</label>
          <select id="downgrade-version" value={downgradeVersion} onChange={(event) => setDowngradeVersion(event.target.value)} disabled={Boolean(busy)}>
            {versionOptions.map((version) => <option key={version}>{version}</option>)}
          </select>
          <label className="field-label" htmlFor="downgrade-destination">隔离包目标目录</label>
          <input id="downgrade-destination" value={downgradeDestination} onChange={(event) => setDowngradeDestination(event.target.value)} placeholder="例如 E:\\CodexSwitchDowngrade" disabled={Boolean(busy)} />
          <button className="ghost-button full" onClick={handleExportDowngrade} disabled={Boolean(busy || !migrationReady || !writersClosed || !downgradeDestination.trim())}><Download className="button-icon" aria-hidden="true" />生成隔离降级包</button>
          <label className="field-label" htmlFor="downgrade-package">使用过的降级包目录</label>
          <input id="downgrade-package" value={downgradePackage} onChange={(event) => setDowngradePackage(event.target.value)} placeholder="选择再次升级时要导入的包" disabled={Boolean(busy)} />
          <button className="ghost-button full" onClick={handleImportDowngrade} disabled={Boolean(busy || !migrationReady || !writersClosed || !downgradePackage.trim())}><ArchiveRestore className="button-icon" aria-hidden="true" />导入旧版本新增会话</button>
        </section>

        <section className="storage-workflow-card" aria-label="待恢复会话">
          <CardHeading icon={<RotateCcw aria-hidden="true" />} eyebrow="RECOVERY INBOX" title="待恢复会话" />
          <p>旧备份中主库没有的会话会先进入待恢复区，不会自动写入 canonical。</p>
          <p className="storage-warning-copy"><CircleAlert aria-hidden="true" />恢复包只保留 7 天；整理前必须关闭所有 Codex 写入进程。</p>
          <button
            ref={legacyReconciliationTriggerRef}
            className="ghost-button full"
            onClick={() => setLegacyReconciliationConfirmOpen(true)}
            disabled={Boolean(busy || !migrationReady || !writersClosed || legacyReconciliationConfirmOpen)}
          >
            <ArchiveRestore className="button-icon" aria-hidden="true" />验证并整理旧备份
          </button>
          {legacyReconciliationConfirmOpen ? (
            <section className="inline-confirmation danger-confirmation" aria-labelledby="legacy-reconciliation-confirmation-title">
              <CircleAlert className="section-icon" aria-hidden="true" />
              <div className="confirmation-copy">
                <p className="eyebrow">REVIEW DESTRUCTIVE ACTION</p>
                <h3
                  id="legacy-reconciliation-confirmation-title"
                  ref={legacyReconciliationConfirmationRef}
                  tabIndex={-1}
                >
                  确认提取并删除旧备份
                </h3>
                <p>将逐份验证旧版完整备份，把独有会话提取到待恢复区，并删除已证明可安全整理的原备份。无法完整验证、存在分叉或来源不明的备份会保留。</p>
              </div>
              <div className="confirmation-actions">
                <button className="ghost-button" onClick={closeLegacyReconciliationConfirmation} disabled={Boolean(busy)}>取消</button>
                <button className="ghost-button danger" onClick={handleReconcileLegacyBackups} disabled={Boolean(busy || !writersClosed)}>
                  <Trash2 className="button-icon" aria-hidden="true" />确认提取并删除可安全整理的旧备份
                </button>
              </div>
            </section>
          ) : null}
          {pendingRecoveryLoading ? <p>正在读取待恢复摘要…</p> : pendingRecovery?.entries.length ? (
            <div className="storage-recovery-list">
              {pendingRecovery.entries.map((entry) => (
                <PendingRecoveryCard
                  key={entry.entryId}
                  entry={entry}
                  disabled={Boolean(busy)}
                  writersClosed={writersClosed}
                  onAction={(action) => handlePendingRecovery(entry, action)}
                />
              ))}
            </div>
          ) : <p className="storage-safe-copy"><ShieldCheck aria-hidden="true" />当前没有待恢复会话。不可读取或来源不明的文件只报告，不自动修复。</p>}
          {pendingRecovery?.expiredPackageCount ? <p className="storage-warning-copy"><CircleAlert aria-hidden="true" />有 {pendingRecovery.expiredPackageCount} 个恢复包已到期，等待安全回收。</p> : null}
          {pendingRecovery?.invalidPackageCount ? <p className="storage-warning-copy"><CircleAlert aria-hidden="true" />有 {pendingRecovery.invalidPackageCount} 个恢复包未通过完整性检查，已阻止恢复和删除。</p> : null}
        </section>

        {lastReceipt ? (
          <section className="storage-workflow-card storage-receipt-card" aria-label="最近操作回执">
            <CardHeading icon={<Check aria-hidden="true" />} eyebrow="LOCAL RECEIPT" title="最近操作已留本地回执" />
            {unclassifiedRestoreReceipt ? (
              <div className="storage-preflight-details">
                <p className="storage-warning-copy">
                  <CircleAlert aria-hidden="true" />未分类载荷已保留：{numberFormat.format(unclassifiedRestoreReceipt.unclassifiedRecoveryCount)} 个，{formatBytes(unclassifiedRestoreReceipt.unclassifiedRecoveryBytes)}。不会自动到期删除，需要交给 Codex 排查。
                </p>
                {unclassifiedRestoreReceipt.unclassifiedRecoveryPaths.length ? (
                  <ul aria-label="未分类载荷相对路径">
                    {unclassifiedRestoreReceipt.unclassifiedRecoveryPaths.map((path, index) => (
                      <li key={`${path}-${index}`}><code>{safeRelativeReceiptPath(path)}</code></li>
                    ))}
                  </ul>
                ) : null}
              </div>
            ) : null}
            <pre>{JSON.stringify(safeReceiptView(lastReceipt), null, 2)}</pre>
          </section>
        ) : null}
      </div>
    </section>
  );
}

function StorageMetric({ label, value, tone = 'normal' }: { label: string; value: string; tone?: 'normal' | 'warning' }) {
  return <div className={`storage-metric ${tone}`}><span>{label}</span><strong>{value}</strong></div>;
}

function CardHeading({ icon, eyebrow, title }: { icon: ReactNode; eyebrow: string; title: string }) {
  return <div className="card-title-row"><span className="section-icon">{icon}</span><div><p className="eyebrow">{eyebrow}</p><h2>{title}</h2></div></div>;
}

function MigrationPreflightDetails({ report }: { report: MigrationPreflightReport }) {
  return (
    <div className="storage-preflight-details">
      <dl>
        <div><dt>会话文件</dt><dd>{numberFormat.format(report.sessionFileCount)}</dd></div>
        <div><dt>provider 副本</dt><dd>{numberFormat.format(report.providerCopyCount)}</dd></div>
        <div><dt>冲突 / 异常</dt><dd>{numberFormat.format(report.conflictCount)} / {numberFormat.format(report.anomalyCount)}</dd></div>
        <div><dt>预计释放</dt><dd>{formatBytes(report.estimatedReclaimBytes)}</dd></div>
        <div><dt>备份需要</dt><dd>{formatBytes(report.requiredBackupBytes)}</dd></div>
        <div><dt>备份可用</dt><dd>{formatBytes(report.availableBackupBytes)}</dd></div>
      </dl>
      {report.blockers.length ? <p className="storage-warning-copy"><CircleAlert aria-hidden="true" />安全检查阻断：{report.blockers.join('、')}</p> : <p className="storage-safe-copy"><ShieldCheck aria-hidden="true" />预检通过，可创建完整备份。</p>}
    </div>
  );
}

function ConflictCard({
  conflict,
  disabled,
  onAction,
}: {
  conflict: SessionConflictSummary;
  disabled: boolean;
  onAction: (action: ConflictResolutionAction) => void;
}) {
  return (
    <article className="storage-conflict-item">
      <header><strong>冲突 {conflict.conflictId.slice(-8)}</strong><span>{conflict.relation}</span></header>
      <div className="storage-conflict-versions">
        <dl>
          <dt>当前主版本</dt>
          <dd>{conflict.currentMessageCount} 条有效消息（新增 {conflict.currentAddedMessageCount}）</dd>
          <dd>{conflict.currentLastMessageAt ?? '时间不可靠'} · {conflict.currentProvider ?? 'provider 未知'} · {conflict.currentOrigin}</dd>
        </dl>
        <dl>
          <dt>候选版本</dt>
          <dd>{conflict.candidateMessageCount} 条有效消息（新增 {conflict.candidateAddedMessageCount}）</dd>
          <dd>{conflict.candidateLastMessageAt ?? '时间不可靠'} · {conflict.candidateProvider ?? 'provider 未知'} · {conflict.candidateOrigin}</dd>
        </dl>
      </div>
      <div className="storage-action-row">
        <button className="ghost-button" onClick={() => onAction('defer')} disabled={disabled || conflict.deferred}>{conflict.deferred ? '已暂不覆盖' : '暂不覆盖'}</button>
        {conflict.newerVersion ? <button className="warm-button" onClick={() => onAction('useNewer')} disabled={disabled}>使用较新版本覆盖</button> : <span className="storage-time-warning">时间不可靠，不提供覆盖推荐</span>}
      </div>
    </article>
  );
}

const pendingRecoveryRelationLabels: Record<PendingRecoverySummary['relation'], string> = {
  missingFromCanonical: '主库缺失',
  extendsCanonical: '完整延续',
  divergent: '真实分叉',
  unknown: '关系未知',
};

const pendingRecoveryStatusLabels: Record<PendingRecoverySummary['status'], string> = {
  pending: '待处理',
  restored: '已恢复',
  deferred: '暂不恢复',
};

function PendingRecoveryCard({
  entry,
  disabled,
  writersClosed,
  onAction,
}: {
  entry: PendingRecoverySummary;
  disabled: boolean;
  writersClosed: boolean;
  onAction: (action: 'restore' | 'defer') => void;
}) {
  const pending = entry.status === 'pending';
  const canRestore = pending && entry.restoreAllowed;
  return (
    <article className={`storage-recovery-item relation-${entry.relation}`}>
      <header>
        <strong>会话 {entry.threadId.slice(-8)}</strong>
        <span>{pendingRecoveryRelationLabels[entry.relation]}</span>
      </header>
      <dl className="storage-recovery-metadata">
        <div><dt>候选版本</dt><dd>{numberFormat.format(entry.candidateMessageCount)} 条有效消息（新增 {numberFormat.format(entry.candidateAddedMessageCount)}）</dd></div>
        <div><dt>当前主版本</dt><dd>{numberFormat.format(entry.currentMessageCount)} 条有效消息（新增 {numberFormat.format(entry.currentAddedMessageCount)}）</dd></div>
        <div><dt>候选来源</dt><dd>{entry.candidateProvider ?? 'provider 未知'} · 备份 {entry.sourceBackupId.slice(-8)}</dd></div>
        <div><dt>最后消息</dt><dd>{entry.candidateLastMessageAt ?? '时间不可靠'}</dd></div>
        <div><dt>正文大小</dt><dd>{formatBytes(entry.payloadBytes)}</dd></div>
        <div><dt>本地到期</dt><dd>{formatLocalTimestamp(entry.expiresAtMs)}</dd></div>
      </dl>
      {entry.relation === 'divergent' ? <p className="storage-time-warning">真实分叉，默认不覆盖；已在上方冲突流程中统一处理。</p> : null}
      {entry.relation === 'unknown' ? <p className="storage-time-warning">内容关系无法可靠判断，已阻止自动恢复。</p> : null}
      <div className="storage-action-row">
        <span className="storage-recovery-status">{pendingRecoveryStatusLabels[entry.status]}</span>
        {pending ? <button className="ghost-button" onClick={() => onAction('defer')} disabled={disabled}>暂不恢复</button> : null}
        {canRestore ? (
          <button className="warm-button" onClick={() => onAction('restore')} disabled={disabled || !writersClosed}>
            恢复到 canonical
          </button>
        ) : null}
      </div>
      {canRestore && !writersClosed ? <p className="storage-time-warning">关闭全部 Codex 写入进程后才能恢复。</p> : null}
    </article>
  );
}

function formatNumber(value: number | undefined) {
  return value === undefined ? '—' : numberFormat.format(value);
}

function formatBytes(bytes: number) {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  let value = bytes;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value >= 10 || index === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[index]}`;
}

function formatLocalTimestamp(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '时间未知';
  return new Date(value).toLocaleString('zh-CN', { hour12: false });
}

function errorMessage(reason: unknown) {
  if (reason instanceof Error) return reason.message;
  if (typeof reason === 'string') return reason;
  return '操作失败，请查看本地诊断。';
}

function isShadowScanAlreadyRunning(reason: unknown) {
  return errorMessage(reason).toLowerCase().includes(shadowScanAlreadyRunningMessage);
}

function isRestoreImportReceipt(receipt: StorageActionReceipt | null): receipt is RestoreImportReceipt {
  return Boolean(receipt && 'unclassifiedRecoveryCount' in receipt);
}

function restoreImportNotice(message: string, receipt: RestoreImportReceipt) {
  if (receipt.unclassifiedRecoveryCount <= 0) return message;
  return `${message} 未分类载荷已保留（${numberFormat.format(receipt.unclassifiedRecoveryCount)} 个，${formatBytes(receipt.unclassifiedRecoveryBytes)}）；不会自动到期删除，需要交给 Codex 排查。`;
}

function safeRelativeReceiptPath(value: string) {
  const path = value.trim();
  const normalized = path.replace(/\\/g, '/');
  const isAbsoluteOrUnsafe = !path
    || /^[a-z]:/i.test(path)
    || /^[\\/]/.test(path)
    || /^[a-z][a-z0-9+.-]*:\/\//i.test(path)
    || path.includes(':')
    || normalized.includes('//')
    || normalized.split('/').some((segment) => segment === '..');
  return isAbsoluteOrUnsafe ? '[absolute path omitted]' : path;
}

function safeReceiptView(receipt: StorageActionReceipt) {
  return Object.fromEntries(Object.entries(receipt).map(([key, value]) => {
    if (key === 'unclassifiedRecoveryPaths' && Array.isArray(value)) {
      return [key, value.map((path) => (
        typeof path === 'string' ? safeRelativeReceiptPath(path) : '[invalid relative path omitted]'
      ))];
    }
    const pathLike = /(path|dir|root|destination)/i.test(key);
    if (!pathLike) return [key, value];
    if (typeof value === 'string') return [key, '[local path]'];
    if (Array.isArray(value)) return [key, value.map(() => '[local path]')];
    return [key, value];
  }));
}
