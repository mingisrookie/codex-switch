import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  ArrowLeftRight,
  Bug,
  Check,
  CircleAlert,
  CloudCog,
  Database,
  Download,
  HardDriveDownload,
  History,
  KeyRound,
  LoaderCircle,
  LogIn,
  MessagesSquare,
  Power,
  RefreshCw,
  RotateCcw,
  Save,
  Settings2,
  ShieldCheck,
  Trash2,
  UserRound,
  Wrench,
  X,
  Zap,
} from 'lucide-react';
import {
  checkForUpdates as defaultCheckForUpdates,
  cleanupAutomaticCheckpoints,
  closeCodexProcesses,
  createFullBackup,
  deleteBackup,
  importPlusRuntime,
  getAppStatus as defaultGetAppStatus,
  getMobileContinuityStatus,
  getUpdateStartupNotice as defaultGetUpdateStartupNotice,
  installUpdate as defaultInstallUpdate,
  launchChatgpt,
  listCodexProcesses,
  loadBackupDashboard as defaultLoadBackupDashboard,
  loadRuntimeDashboard as defaultLoadRuntimeDashboard,
  loadSessionDashboard as defaultLoadSessionDashboard,
  loadingDashboard,
  requestAppExit as defaultRequestAppExit,
  acknowledgeMobileContinuityNotice,
  publishMobileContinuitySession,
  recordFrontendDiagnostic,
  restoreBackup,
  restoreSessionsVisible,
  scanSessionStorage as defaultScanSessionStorage,
  switchRuntime,
  setMobileContinuityEnabled,
  mergeAndRepairSessions,
  upsertRelayRuntime,
} from './api';
import { OperationResultPanel, type OperationView } from './OperationResultPanel';
import { DiagnosticExportAction, DiagnosticPanel } from './DiagnosticPanel';
import { RelayRuntimeDialog } from './RelayRuntimeDialog';
import {
  RuntimeSwitchProgressPanel,
  type RuntimeSwitchFlow,
} from './RuntimeSwitchProgressPanel';
import { SessionManagementPage } from './SessionManagementPage';
import { SessionStorageManagementPage } from './SessionStorageManagementPage';
import { SkillsManagementPage } from './SkillsManagementPage';
import type {
  AppExitRequestResult,
  BackupSummary,
  BackupDashboardData,
  CheckpointCleanupReceipt,
  CheckpointStorageStatus,
  DashboardData,
  DomainState,
  MobileContinuityStatus,
  RelayRuntimeInput,
  RuntimeKind,
  RuntimeMetadata,
  RuntimeStatus,
  RuntimeDashboardData,
  RuntimeSwitchProgress,
  RuntimeSwitchResult,
  SessionDashboardData,
  OperationRecord,
  SessionMutationResult,
  ShadowScanReport,
  SessionSyncProgress,
  SessionSyncResult,
  UpdateCheckResult,
} from './types';

type AppProps = {
  loadDashboard?: () => Promise<DashboardData>;
  loadRuntimeDashboard?: () => Promise<RuntimeDashboardData>;
  loadSessionDashboard?: () => Promise<SessionDashboardData>;
  loadBackupDashboard?: () => Promise<BackupDashboardData>;
  scanSessionStorage?: () => Promise<ShadowScanReport>;
  registerCloseGuard?: RegisterCloseGuard;
  requestExit?: () => Promise<AppExitRequestResult>;
};

type CloseGuardEvent = { preventDefault: () => void };
type RegisterCloseGuard = (
  onCloseRequested: (event: CloseGuardEvent) => void,
) => Promise<() => void>;

type PendingConfirmation =
  | { kind: 'importAccount' }
  | { kind: 'syncSessions' }
  | { kind: 'restoreBackup'; backup: BackupSummary }
  | { kind: 'deleteBackup'; backup: BackupSummary };

type RefreshScope = 'dashboard' | 'runtime' | 'backup' | 'none';
type CheckpointCleanupFlow = {
  status: 'running' | 'succeeded' | 'partial' | 'failed';
  startedAtMs: number;
  completedAtMs?: number;
  receipt?: CheckpointCleanupReceipt;
  error?: string;
  operationId?: string;
};
type OperationFailureView = { message: string; operationId?: string };
const numberFormat = new Intl.NumberFormat('zh-CN');

async function defaultRegisterCloseGuard(
  onCloseRequested: (event: CloseGuardEvent) => void,
) {
  if (typeof window === 'undefined' || !('__TAURI_INTERNALS__' in window)) {
    return () => undefined;
  }
  const { getCurrentWindow } = await import('@tauri-apps/api/window');
  return getCurrentWindow().onCloseRequested(onCloseRequested);
}

function App({
  loadDashboard,
  loadRuntimeDashboard = defaultLoadRuntimeDashboard,
  loadSessionDashboard = defaultLoadSessionDashboard,
  loadBackupDashboard = defaultLoadBackupDashboard,
  scanSessionStorage = defaultScanSessionStorage,
  registerCloseGuard = defaultRegisterCloseGuard,
  requestExit = defaultRequestAppExit,
}: AppProps) {
  const [data, setData] = useState<DashboardData>(() => loadingDashboard());
  const [error, setError] = useState<OperationFailureView | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [receipt, setReceipt] = useState<OperationView | null>(null);
  const [relayEditorOpen, setRelayEditorOpen] = useState(false);
  const [diagnosticPanelOpen, setDiagnosticPanelOpen] = useState(false);
  const [diagnosticBusy, setDiagnosticBusy] = useState(false);
  const [relaySubmitError, setRelaySubmitError] = useState<OperationFailureView | null>(null);
  const [mobileContinuity, setMobileContinuity] = useState<MobileContinuityStatus | null>(null);
  const [mobileContinuityLoading, setMobileContinuityLoading] = useState(true);
  const [pendingConfirmation, setPendingConfirmation] = useState<PendingConfirmation | null>(null);
  const [switchFlow, setSwitchFlow] = useState<RuntimeSwitchFlow | null>(null);
  const [sessionsStale, setSessionsStale] = useState(() => loadDashboard === undefined);
  const [backupsStale, setBackupsStale] = useState(() => loadDashboard === undefined);
  const [backupLoading, setBackupLoading] = useState(false);
  const [storageScanning, setStorageScanning] = useState(false);
  const [runtimeRefreshPending, setRuntimeRefreshPending] = useState(false);
  const [checkpointCleanupFlow, setCheckpointCleanupFlow] =
    useState<CheckpointCleanupFlow | null>(null);
  const [sessionRevision, setSessionRevision] = useState(0);
  const [activePage, setActivePage] = useState<'runtime' | 'sessions' | 'storage' | 'skills'>('runtime');
  const [appVersion, setAppVersion] = useState<string | null>(null);
  const [updateResult, setUpdateResult] = useState<UpdateCheckResult | null>(null);
  const [updateChecking, setUpdateChecking] = useState(false);
  const [updateInstalling, setUpdateInstalling] = useState(false);
  const [updateError, setUpdateError] = useState<OperationFailureView | null>(null);
  const [updateNotice, setUpdateNotice] = useState<string | null>(null);
  const [startupUpdateError, setStartupUpdateError] = useState<string | null>(null);
  const [closeGuardStatus, setCloseGuardStatus] =
    useState<'loading' | 'ready' | 'failed'>('loading');
  const [closePending, setClosePending] = useState(false);
  const [continuityCloseChoice, setContinuityCloseChoice] =
    useState<'pending' | 'exit' | null>(null);
  const [dismissedUpdateVersion, setDismissedUpdateVersion] = useState<string | null>(null);
  const loadRequestId = useRef(0);
  const runtimeRequestId = useRef(0);
  const sessionRequestId = useRef(0);
  const backupRequestId = useRef(0);
  const backupLoadInFlight = useRef<Promise<void> | null>(null);
  const backupRefreshQueued = useRef(false);
  const checkpointCleanupInFlight = useRef(false);
  const requestedSessionRevision = useRef(-1);
  const switchAttemptId = useRef(0);
  const switchTrigger = useRef<HTMLElement | null>(null);
  const switchFocusRestorePending = useRef(false);
  const launchRetryInFlight = useRef(false);
  const confirmationTrigger = useRef<HTMLElement | null>(null);
  const startupCheckStarted = useRef(false);
  const updateCheckInFlight = useRef(false);
  const exclusiveActionInFlight = useRef(false);
  const childActionInFlight = useRef(false);
  const closeRequestInFlight = useRef(false);
  const exitScheduled = useRef(false);
  const continuitySyncActive = switchFlow?.status === 'running'
    && switchFlow.events.some((event) => event.phase === 'syncingIncrementalSessions');
  const continuitySyncActiveRef = useRef(false);
  const continuityCloseChoiceRef = useRef<'pending' | 'exit' | null>(null);
  continuitySyncActiveRef.current = continuitySyncActive;
  continuityCloseChoiceRef.current = continuityCloseChoice;

  const attemptAppExit = useCallback(async () => {
    if (exclusiveActionInFlight.current || childActionInFlight.current) {
      setClosePending(true);
      return;
    }
    if (closeRequestInFlight.current) return;
    closeRequestInFlight.current = true;
    try {
      const result = await requestExit();
      exitScheduled.current = result.scheduled;
      setClosePending(true);
    } catch (reason) {
      exitScheduled.current = false;
      setClosePending(false);
      const failure = operationFailure(reason);
      setError({ ...failure, message: `关闭 ChatGPT Switch 失败：${failure.message}` });
    } finally {
      closeRequestInFlight.current = false;
    }
  }, [requestExit]);

  useEffect(() => {
    let cancelled = false;
    const requestId = ++loadRequestId.current;
    const initialLoad = loadDashboard
      ? loadDashboard()
      : loadRuntimeDashboard().then((next) => ({ ...loadingDashboard(), ...next }));
    initialLoad
      .then((next) => {
        if (!cancelled && requestId === loadRequestId.current) {
          setData(next);
          if (loadDashboard) {
            setSessionsStale(false);
            setBackupsStale(false);
          }
        }
      })
      .catch((reason: unknown) => {
        if (!cancelled && requestId === loadRequestId.current) setError(operationFailure(reason));
      });
    return () => { cancelled = true; };
  }, [loadDashboard, loadRuntimeDashboard]);

  useEffect(() => {
    if (startupCheckStarted.current) return;
    startupCheckStarted.current = true;
    void defaultGetAppStatus()
      .then((status) => setAppVersion(status.version))
      .catch(() => undefined);
    void defaultGetUpdateStartupNotice()
      .then((notice) => {
        if (notice?.status === 'updated') setUpdateNotice('更新完成，已启动新版本。');
        if (notice?.status === 'rolledBack') setStartupUpdateError('更新启动失败，已恢复并重新启动旧版本。');
      })
      .catch(() => undefined);
    void runUpdateCheck(false);
    void getMobileContinuityStatus()
      .then(setMobileContinuity)
      .catch((reason: unknown) => {
        setError({
          message: `Relay 会话视图状态读取失败：${errorMessage(reason)}。请求端切换不会恢复旧 provider 正文复制。`,
        });
      })
      .finally(() => setMobileContinuityLoading(false));
  }, []);

  useEffect(() => {
    const record = (
      eventKind: 'unhandledError' | 'unhandledRejection',
      errorCode: 'frontend.unhandled_error' | 'frontend.unhandled_rejection',
      safeMessage: string,
    ) => {
      void recordFrontendDiagnostic({
        level: 'error',
        component: 'frontend',
        eventKind,
        errorCode,
        safeMessage,
      }).catch(() => undefined);
    };
    const handleError = () => record(
      'unhandledError',
      'frontend.unhandled_error',
      '前端发生未处理异常',
    );
    const handleRejection = () => record(
      'unhandledRejection',
      'frontend.unhandled_rejection',
      '前端发生未处理 Promise 拒绝',
    );
    window.addEventListener('error', handleError);
    window.addEventListener('unhandledrejection', handleRejection);
    return () => {
      window.removeEventListener('error', handleError);
      window.removeEventListener('unhandledrejection', handleRejection);
    };
  }, []);

  useEffect(() => {
    if (switchFlow || runtimeRefreshPending || !switchFocusRestorePending.current) return;
    const trigger = switchTrigger.current;
    if (!trigger?.isConnected) {
      switchFocusRestorePending.current = false;
      return;
    }
    trigger.focus({ preventScroll: true });
    if (document.activeElement === trigger) {
      switchFocusRestorePending.current = false;
      return;
    }
    const runtimeCard = trigger.closest<HTMLElement>('.runtime-card');
    runtimeCard?.focus({ preventScroll: true });
    switchFocusRestorePending.current = false;
  }, [runtimeRefreshPending, switchFlow]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void registerCloseGuard((event) => {
      event.preventDefault();
      setClosePending(true);
      if (
        continuitySyncActiveRef.current
        && continuityCloseChoiceRef.current !== 'exit'
      ) {
        continuityCloseChoiceRef.current = 'pending';
        setContinuityCloseChoice('pending');
        return;
      }
      void attemptAppExit();
    })
      .then((stopListening) => {
        if (disposed) {
          stopListening();
        } else {
          unlisten = stopListening;
          setCloseGuardStatus('ready');
        }
      })
      .catch(() => {
        if (!disposed) setCloseGuardStatus('failed');
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [attemptAppExit, registerCloseGuard]);

  useEffect(() => {
    if (!closePending) return undefined;
    const retry = window.setInterval(() => {
      if (continuityCloseChoiceRef.current === 'pending') return;
      if (!exitScheduled.current) void attemptAppExit();
    }, 500);
    const recover = window.setTimeout(() => {
      if (!exitScheduled.current) return;
      exitScheduled.current = false;
      void attemptAppExit();
    }, 2_500);
    return () => {
      window.clearInterval(retry);
      window.clearTimeout(recover);
    };
  }, [attemptAppExit, closePending]);

  useEffect(() => {
    if (
      activePage !== 'sessions'
      || !sessionsStale
      || busy !== null
      || updateInstalling
      || switchFlow?.status === 'running'
    ) return;
    if (requestedSessionRevision.current === sessionRevision) return;
    requestedSessionRevision.current = sessionRevision;
    void refreshSessionDomains().catch((reason: unknown) => setError(operationFailure(reason)));
  }, [activePage, busy, sessionRevision, sessionsStale, switchFlow?.status, updateInstalling]);

  const codexHome = readyData(data.codexHome);
  const sessions = readyData(data.sessions);
  const managedSessions = readyData(data.managedSessions);
  const sessionStorage = readyData(data.sessionStorage);
  const runtimes = readyData(data.runtimes);
  const runtimeStatus = readyData(data.runtimeStatus);
  const plusRuntime = useMemo(() => runtimes?.find((runtime) => runtime.kind === 'plus') ?? null, [runtimes]);
  const relayRuntime = useMemo(() => runtimes?.find((runtime) => runtime.kind === 'relay') ?? null, [runtimes]);

  const officialAuthReady = Boolean(
    codexHome?.authJson.exists
      && (codexHome.authSummary?.authMode === 'chatgpt' || runtimeStatus?.authMode === 'chatgpt'),
  );
  const canImportAccount = officialAuthReady
    && Boolean(codexHome?.configToml.exists)
    && data.runtimes.status === 'ready';
  const canConfigureRelay = data.runtimes.status === 'ready';
  const canSwitchRuntime = data.runtimes.status === 'ready'
    && closeGuardStatus === 'ready'
    && !runtimeRefreshPending;
  const canSwitchAccount = canSwitchRuntime && officialAuthReady;
  const canSwitchRelay = canSwitchRuntime;
  const runtimeGateHelp = closeGuardStatus === 'loading'
    ? '正在确认本机 Codex 写入进程，请稍后重试。'
    : closeGuardStatus === 'failed'
      ? '无法确认本机 Codex 写入进程；为避免并发写入，当前已阻止切换。'
    : runtimeRefreshPending
      ? '正在刷新当前运行态。'
      : data.runtimes.status !== 'ready'
        ? '运行态槽位尚未加载完成。'
        : null;
  const canSync = !sessionsStale
    && data.sessions.status === 'ready'
    && data.managedSessions.status === 'ready'
    && Boolean(sessionStorage)
    && sessionStorage?.migrationRequired === false
    && sessionStorage.status !== 'reviewRequired';
  const canMutateSessions = data.managedSessions.status === 'ready';
  const canRestoreBackup = !backupsStale && data.backups.status === 'ready';
  // Startup continuity initialization owns the same backend mutation guard as
  // route/config writes. Keep every mutation control disabled until that one
  // background mutation settles so an immediate click cannot lose a try-lock
  // race and surface a spurious "another mutation" failure.
  const exclusiveBusy = busy !== null || updateInstalling || mobileContinuityLoading;

  function handleChildBusyChange(label: string | null) {
    childActionInFlight.current = label !== null;
    setBusy(label);
  }
  const threadCount = sessionsStale
    ? '待刷新'
    : sessions ? numberFormat.format(sessions.threadCount) : statusLabel(data.sessions);
  const jsonlCount = sessionsStale
    ? '待刷新'
    : sessions ? numberFormat.format(sessions.sessionJsonlCount) : statusLabel(data.sessions);

  async function refresh() {
    if (!loadDashboard) {
      await Promise.all([
        refreshRuntimeDomains(),
        refreshSessionDomains(),
        refreshBackupDomains(),
      ]);
      return;
    }
    const requestId = ++loadRequestId.current;
    runtimeRequestId.current += 1;
    sessionRequestId.current += 1;
    backupRequestId.current += 1;
    const next = await loadDashboard();
    if (requestId === loadRequestId.current) {
      setData(next);
      setSessionsStale(false);
      setBackupsStale(false);
    }
  }

  async function refreshRuntimeDomains() {
    const requestId = ++runtimeRequestId.current;
    setRuntimeRefreshPending(true);
    try {
      const next = await loadRuntimeDashboard();
      if (requestId === runtimeRequestId.current) {
        setData((current) => ({ ...current, ...next }));
      }
    } finally {
      if (requestId === runtimeRequestId.current) {
        setRuntimeRefreshPending(false);
      }
    }
  }

  async function refreshSessionDomains() {
    const requestId = ++sessionRequestId.current;
    const next = await loadSessionDashboard();
    if (requestId === sessionRequestId.current) {
      setData((current) => ({ ...current, ...next }));
      setSessionsStale(false);
    }
  }

  async function handleSessionStorageScan() {
    if (storageScanning) return;
    setStorageScanning(true);
    setError(null);
    try {
      const report = await scanSessionStorage();
      setData((current) => ({
        ...current,
        sessionStorage: { status: 'ready', data: report },
      }));
    } catch (reason) {
      const failure = operationFailure(reason);
      setError({ ...failure, message: `会话存储扫描失败：${failure.message}` });
    } finally {
      setStorageScanning(false);
    }
  }

  function refreshBackupDomains() {
    if (backupLoadInFlight.current) {
      backupRefreshQueued.current = true;
      return backupLoadInFlight.current;
    }
    const requestId = ++backupRequestId.current;
    setBackupLoading(true);
    const task = loadBackupDashboard()
      .then((next) => {
        if (requestId === backupRequestId.current) {
          setData((current) => ({ ...current, ...next }));
          setBackupsStale(false);
        }
      })
      .finally(() => {
        if (backupLoadInFlight.current === task) {
          backupLoadInFlight.current = null;
        }
        setBackupLoading(false);
        if (backupRefreshQueued.current) {
          backupRefreshQueued.current = false;
          void refreshBackupDomains().catch((reason: unknown) => setError({
            message: `备份状态刷新失败：${errorMessage(reason)}`,
          }));
        }
      });
    backupLoadInFlight.current = task;
    return task;
  }

  function refreshInBackground(
    scope: RefreshScope = 'dashboard',
    onFailure?: (reason: unknown) => void,
  ) {
    if (scope === 'none') return;
    const task = scope === 'runtime'
      ? refreshRuntimeDomains()
      : scope === 'backup'
        ? refreshBackupDomains()
        : refresh();
    void task.catch((reason: unknown) => onFailure?.(reason));
  }

  function markSessionsStale() {
    loadRequestId.current += 1;
    sessionRequestId.current += 1;
    setSessionsStale(true);
    setSessionRevision((current) => current + 1);
  }

  function markBackupsStale() {
    loadRequestId.current += 1;
    backupRequestId.current += 1;
    setBackupsStale(true);
  }

  function handleManualRefresh() {
    if (exclusiveBusy || exclusiveActionInFlight.current) return;
    setError(null);
    if (activePage === 'sessions') {
      void refreshSessionDomains().catch((reason: unknown) => setError(operationFailure(reason)));
      return;
    }
    refreshInBackground('runtime', (reason) => setError(operationFailure(reason)));
  }

  function handleLoadBackups() {
    if (exclusiveBusy || exclusiveActionInFlight.current || backupLoadInFlight.current) return;
    setData((current) => ({
      ...current,
      backups: { status: 'loading' },
      backupStorage: { status: 'loading' },
    }));
    void refreshBackupDomains().catch((reason: unknown) => setError(operationFailure(reason)));
  }

  function requestConfirmation(pending: PendingConfirmation) {
    confirmationTrigger.current = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
    setPendingConfirmation(pending);
  }

  function cancelPendingConfirmation() {
    setPendingConfirmation(null);
    window.requestAnimationFrame(() => confirmationTrigger.current?.focus());
  }

  async function runUpdateCheck(reportFailure: boolean) {
    if (updateCheckInFlight.current) return;
    updateCheckInFlight.current = true;
    setUpdateChecking(true);
    if (reportFailure) setUpdateError(null);
    try {
      const result = await defaultCheckForUpdates();
      setAppVersion(result.currentVersion);
      setUpdateResult(result);
      setUpdateError(null);
    } catch (reason) {
      if (reportFailure) setUpdateError({ message: errorMessage(reason) });
    } finally {
      updateCheckInFlight.current = false;
      setUpdateChecking(false);
    }
  }

  async function handleInstallUpdate() {
    if (updateInstalling || busy !== null || exclusiveActionInFlight.current) return;
    exclusiveActionInFlight.current = true;
    setUpdateInstalling(true);
    setUpdateError(null);
    setUpdateNotice(null);
    try {
      const result = await defaultInstallUpdate();
      setUpdateNotice(`v${result.toVersion} 已下载并校验，正在重启完成更新…`);
    } catch (reason) {
      setUpdateError(operationFailure(reason));
      setUpdateInstalling(false);
      exclusiveActionInFlight.current = false;
    }
  }

  async function runAction<T>(
    label: string,
    action: () => Promise<T>,
    view: (result: T) => OperationView,
    onFailure?: (message: string, operationId?: string) => void,
    refreshScope: RefreshScope = 'dashboard',
    onStart?: () => void,
    reportFailureGlobally = true,
  ) {
    if (busy !== null || updateInstalling || exclusiveActionInFlight.current) return null;
    exclusiveActionInFlight.current = true;
    onStart?.();
    setBusy(label);
    setError(null);
    setReceipt(null);
    try {
      let result: T;
      try {
        result = await action();
      } catch (reason) {
        const failure = operationFailure(reason);
        const message = failure.message;
        if (reportFailureGlobally) setError(failure);
        onFailure?.(message, failure.operationId);
        refreshInBackground(refreshScope);
        return null;
      }
      setReceipt(view(result));
      refreshInBackground(refreshScope, (reason) => {
        setError({ message: `操作已成功，但状态刷新失败：${errorMessage(reason)}` });
      });
      return result;
    } finally {
      exclusiveActionInFlight.current = false;
      setBusy(null);
    }
  }

  async function ensureChatGptClosed(_reason: string) {
    const processes = await listCodexProcesses();
    if (processes.length === 0) return;
    await closeCodexProcesses();
  }

  async function handleImportPlus() {
    if (!canImportAccount) return;
    const overwrite = Boolean(plusRuntime);
    if (overwrite) {
      requestConfirmation({ kind: 'importAccount' });
      return;
    }
    await importAccountRuntime(false);
  }

  async function importAccountRuntime(overwrite: boolean) {
    await runAction('保存 ChatGPT 账号态', () => importPlusRuntime(overwrite), () => ({
      label: 'ChatGPT 账号态已保存', metrics: ['运行态：ChatGPT 账号'],
    }), undefined, 'runtime');
  }

  async function handleSaveRelay(input: RelayRuntimeInput) {
    setRelaySubmitError(null);
    const saved = await runAction('配置中转站', () => upsertRelayRuntime(input), (runtime) => ({
      label: 'API 中转站已保存', metrics: [`模型：${runtime.model ?? '未设置'}`],
    }), (message, operationId) => setRelaySubmitError({ message, operationId }), 'runtime', undefined, false);
    if (saved) setRelayEditorOpen(false);
  }

  async function handleSwitch(
    runtimeId: RuntimeKind,
    label: string,
    trigger?: HTMLElement,
  ) {
    const targetAvailable = runtimeId === 'plus' ? canSwitchAccount : canSwitchRelay;
    if (!targetAvailable || busy !== null || updateInstalling || exclusiveActionInFlight.current) return;
    switchTrigger.current = trigger ?? (
      document.activeElement instanceof HTMLElement ? document.activeElement : null
    );
    exclusiveActionInFlight.current = true;
    const attemptId = ++switchAttemptId.current;
    const startedAtMs = Date.now();
    setBusy(label);
    setError(null);
    setReceipt(null);
    setSwitchFlow({
      status: 'running',
      target: runtimeId,
      events: [],
      startedAtMs,
    });

    try {
      const onProgress = (event: RuntimeSwitchProgress) => {
        if (attemptId !== switchAttemptId.current) return;
        setSwitchFlow((current) => {
          if (!current || current.target !== runtimeId) return current;
          const failedPhase = event.phase === 'rollingBack'
            ? current.failedPhase ?? lastRuntimeWorkPhase(current.events)
            : event.phase === 'failed'
              ? current.failedPhase ?? lastRuntimeWorkPhase(current.events)
              : current.failedPhase;
          const events = [
            ...current.events.filter((item) => item.phase !== event.phase),
            event,
          ];
          return {
            ...current,
            events,
            failedPhase,
            operationId: event.operationId ?? current.operationId,
          };
        });
      };
      const result: RuntimeSwitchResult = await switchRuntime(runtimeId, onProgress);
      if (attemptId !== switchAttemptId.current) return;
      if (result.incrementalSessionSync.status === 'applied') {
        markSessionsStale();
        markBackupsStale();
      } else if (
        result.incrementalSessionSync.status === 'failed'
        || result.incrementalSessionSync.status === 'deferred'
      ) {
        // A failed/deferred apply can retain a fail-closed checkpoint even
        // though the request route itself succeeded.
        markBackupsStale();
      }
      setSwitchFlow((current) => current ? {
        ...current,
        status: 'succeeded',
        operationId: result.operationId,
        result,
        completedAtMs: Date.now(),
      } : current);
      if (runtimeId === 'plus') {
        void getMobileContinuityStatus()
          .then(setMobileContinuity)
          .catch(() => undefined);
      }
      refreshInBackground('runtime', (reason) => {
        setSwitchFlow((current) => current ? {
          ...current,
          refreshError: errorMessage(reason),
        } : current);
      });
    } catch (reason) {
      if (attemptId !== switchAttemptId.current) return;
      const failure = operationFailure(reason);
      setSwitchFlow((current) => current ? {
        ...current,
        status: 'failed',
        error: failure.message,
        operationId: failure.operationId ?? current.operationId,
        failedPhase: current.failedPhase ?? lastRuntimeWorkPhase(current.events),
        completedAtMs: Date.now(),
      } : current);
      refreshInBackground('runtime', (refreshReason) => {
        setSwitchFlow((current) => current ? {
          ...current,
          refreshError: errorMessage(refreshReason),
        } : current);
      });
    } finally {
      if (attemptId === switchAttemptId.current) {
        exclusiveActionInFlight.current = false;
        setBusy(null);
      }
    }
  }

  async function handleRetryChatGptLaunch() {
    const launch = switchFlow?.result?.chatgptLaunch;
    if (
      switchFlow?.status !== 'succeeded'
      || !switchFlow.result
      || launch?.status !== 'failed'
      || !(
        launch.reason === 'activationFailed'
        || launch.reason === 'verificationFailed'
        || !launch.reason
      )
      || launchRetryInFlight.current
      || exclusiveActionInFlight.current
    ) return;
    launchRetryInFlight.current = true;
    exclusiveActionInFlight.current = true;
    setBusy('打开 ChatGPT');
    setSwitchFlow((current) => current ? { ...current, launchRetrying: true } : current);
    try {
      const launch = await launchChatgpt();
      setSwitchFlow((current) => current?.result ? {
        ...current,
        launchRetrying: false,
        result: { ...current.result, chatgptLaunch: launch },
      } : current);
    } catch (reason) {
      setSwitchFlow((current) => current?.result ? {
        ...current,
        launchRetrying: false,
        result: {
          ...current.result,
          chatgptLaunch: { status: 'failed', message: errorMessage(reason) },
        },
      } : current);
    } finally {
      launchRetryInFlight.current = false;
      exclusiveActionInFlight.current = false;
      setBusy(null);
    }
  }

  async function handleMobileContinuityToggle() {
    if (!mobileContinuity || exclusiveBusy) return;
    await runAction(
      mobileContinuity.enabled ? '关闭显式加入视图' : '开启显式加入视图',
      () => setMobileContinuityEnabled(!mobileContinuity.enabled),
      (status) => ({
        label: status.enabled ? '显式加入视图已开启' : '显式加入视图已关闭',
        metrics: ['只更新数据库引用，不复制会话正文'],
      }),
      undefined,
      'none',
    ).then((status) => {
      if (status) setMobileContinuity(status);
    });
  }

  async function handleAcknowledgeMobileContinuity() {
    try {
      setMobileContinuity(await acknowledgeMobileContinuityNotice());
    } catch (reason) {
      setError(operationFailure(reason));
    }
  }

  async function handlePublishMobileContinuitySession(threadId: string) {
    const status = await runAction(
      '加入 Account 视图',
      () => publishMobileContinuitySession(threadId),
      (next) => ({
        label: '会话已加入 Account 视图',
        metrics: [
          `Account 视图：${next.remotePublished}`,
          `兼容状态：${next.partial}`,
        ],
      }),
      undefined,
      'runtime',
    );
    if (status) {
      setMobileContinuity(status);
      markSessionsStale();
      return true;
    }
    return false;
  }

  function closeSwitchTask() {
    if (
      !switchFlow
      || switchFlow.status === 'running'
      || switchFlow.launchRetrying
      || runtimeRefreshPending
    ) return;
    switchFocusRestorePending.current = true;
    setSwitchFlow(null);
    window.requestAnimationFrame(() => {
      if (runtimeRefreshPending) return;
      switchTrigger.current?.focus({ preventScroll: true });
      if (document.activeElement === switchTrigger.current) {
        switchFocusRestorePending.current = false;
      }
    });
  }

  function handleSyncSessions() {
    if (!canSync || busy !== null || updateInstalling || exclusiveActionInFlight.current) return;
    requestConfirmation({ kind: 'syncSessions' });
  }

  async function performSyncSessions() {
    const progressEvents: SessionSyncProgress[] = [];
    await runAction(
      '会话合并与修复',
      async () => {
        try {
          return await mergeAndRepairSessions((event) => {
            progressEvents.push(event);
            setBusy(sessionSyncProgressLabel(event));
          });
        } catch (reason) {
          const operationId = [...progressEvents]
            .reverse()
            .find((event) => event.operationId)?.operationId ?? undefined;
          throw correlatedFailure(reason, operationId);
        }
      },
      (result) => syncReceipt(result, progressEvents),
    );
  }

  async function handleRestoreSessionsVisible(ids: string[]) {
    const result = await runAction('恢复会话可见', async () => {
      await ensureChatGptClosed('恢复会话可见');
      return restoreSessionsVisible(ids);
    }, mutationReceipt('会话恢复完成'));
    return result !== null;
  }

  async function handleRestoreBackup(backup: BackupSummary) {
    if (!backup.verified || !canRestoreBackup) return;
    requestConfirmation({ kind: 'restoreBackup', backup });
  }

  async function performRestoreBackup(backup: BackupSummary) {
    await runAction('恢复备份', async () => {
      await ensureChatGptClosed('备份恢复');
      return restoreBackup(backup.backupDir);
    }, (result) => ({
      label: '备份恢复完成', operationId: result.operationId,
      rolledBack: result.rolledBack, warnings: result.warnings,
      backupCount: result.safetyBackup ? 1 : 0,
      backupPaths: result.safetyBackup ? [result.safetyBackup.backupDir] : [],
      metrics: [`恢复文件：${result.restoredFiles}`],
    }));
  }

  function handleDeleteBackup(backup: BackupSummary) {
    if (!backup.verified || !canRestoreBackup || backupLoadInFlight.current) return;
    requestConfirmation({ kind: 'deleteBackup', backup });
  }

  async function performDeleteBackup(backup: BackupSummary) {
    await runAction('删除恢复点', () => deleteBackup(backup.backupDir, true), (result) => ({
      label: '恢复点已删除',
      operationId: result.operationId,
      warnings: result.warnings,
      metrics: [`已回收：${formatBytes(result.reclaimedBytes)}`],
    }), undefined, 'backup');
  }

  async function handleCreateFullBackup() {
    if (backupLoadInFlight.current) return;
    await runAction('创建完整备份', createFullBackup, (result) => ({
      label: '完整备份已创建',
      operationId: result.operationId,
      backupCount: result.backups.length,
      backupPaths: result.backups.map((backup) => backup.backupDir),
      warnings: result.warnings,
      metrics: [
        '范围：当前 Home + 共享池',
        ...result.backups.map((backup, index) => (
          `来源 ${index + 1}：${backup.sourceRoot} · 受管数据库：${backup.trackedDatabaseCount}`
        )),
      ],
    }), undefined, 'backup');
  }

  async function handleCleanupAutomaticCheckpoints() {
    if (
      checkpointCleanupInFlight.current
      || backupLoadInFlight.current
      || exclusiveBusy
      || exclusiveActionInFlight.current
    ) return;
    checkpointCleanupInFlight.current = true;
    const startedAtMs = Date.now();
    setCheckpointCleanupFlow({ status: 'running', startedAtMs });
    setData((current) => ({
      ...current,
      backupStorage: { status: 'loading' },
    }));
    try {
      const result = await runAction(
        '清理自动检查点',
        cleanupAutomaticCheckpoints,
        (cleanup) => ({
          label: cleanup.failedCount > 0
            ? '自动检查点部分完成'
            : cleanup.warnings.length > 0
              ? '自动检查点清理完成（有保留说明）'
              : '自动检查点清理完成',
          operationId: cleanup.operationId,
          warnings: cleanup.warnings,
          metrics: [
            `计划：${cleanup.attemptedCount}`,
            `失败：${cleanup.failedCount}`,
            `释放：${formatBytes(cleanup.reclaimedBytes)}`,
            `回收检查点：${cleanup.reclaimedCount}`,
            `安全保留：${cleanup.retainedCount}`,
          ],
        }),
        (message, operationId) => {
          setCheckpointCleanupFlow({
            status: 'failed',
            startedAtMs,
            completedAtMs: Date.now(),
            error: message,
            operationId,
          });
        },
        'backup',
        undefined,
        false,
      );
      if (result) {
        setCheckpointCleanupFlow({
          status: result.failedCount > 0 ? 'partial' : 'succeeded',
          startedAtMs,
          completedAtMs: Date.now(),
          receipt: result,
        });
      }
    } finally {
      checkpointCleanupInFlight.current = false;
    }
  }

  async function confirmPendingAction() {
    const pending = pendingConfirmation;
    if (!pending) return;
    setPendingConfirmation(null);
    if (pending.kind === 'importAccount') {
      await importAccountRuntime(true);
    } else if (pending.kind === 'syncSessions') {
      await performSyncSessions();
    } else if (pending.kind === 'restoreBackup') {
      await performRestoreBackup(pending.backup);
    } else {
      await performDeleteBackup(pending.backup);
    }
    window.requestAnimationFrame(() => confirmationTrigger.current?.focus());
  }

  const domainErrors = dashboardErrors(data);
  const updateVisible = Boolean(
    updateResult?.updateAvailable && dismissedUpdateVersion !== updateResult.latestVersion,
  );
  const versionStatus = updateResult
    ? updateResult.updateAvailable
      ? `v${updateResult.currentVersion} · 发现 v${updateResult.latestVersion}`
      : `v${updateResult.currentVersion} · 已是最新版`
    : appVersion
      ? `v${appVersion}`
      : '版本读取中';

  return (
    <>
    <main
      className="app-shell"
      inert={switchFlow ? true : undefined}
      aria-hidden={switchFlow ? true : undefined}
    >
      <header className="topbar">
        <div className="brand-lockup">
          <span className="brand-mark"><ArrowLeftRight aria-hidden="true" /></span>
          <span><strong>CHATGPT SWITCH</strong><small>LOCAL CONTROL SURFACE</small></span>
        </div>
        <nav className="topbar-tabs" aria-label="主导航">
          <button disabled={exclusiveBusy} aria-current={activePage === 'runtime' ? 'page' : undefined} className={`topbar-tab ${activePage === 'runtime' ? 'active' : ''}`} onClick={() => setActivePage('runtime')}><Zap aria-hidden="true" />运行态</button>
          <button disabled={exclusiveBusy} aria-current={activePage === 'sessions' ? 'page' : undefined} className={`topbar-tab ${activePage === 'sessions' ? 'active' : ''}`} onClick={() => setActivePage('sessions')}><MessagesSquare aria-hidden="true" />会话</button>
          <button disabled={exclusiveBusy} aria-current={activePage === 'storage' ? 'page' : undefined} className={`topbar-tab ${activePage === 'storage' ? 'active' : ''}`} onClick={() => setActivePage('storage')}><Database aria-hidden="true" />存储</button>
          <button disabled={exclusiveBusy} aria-current={activePage === 'skills' ? 'page' : undefined} className={`topbar-tab ${activePage === 'skills' ? 'active' : ''}`} onClick={() => setActivePage('skills')}><Wrench aria-hidden="true" />技能</button>
        </nav>
        <div className="topbar-actions">
          <span className="topbar-version" aria-live="polite">{versionStatus}</span>
          <button className="ghost-button" onClick={() => void runUpdateCheck(true)} disabled={updateChecking || exclusiveBusy}>
            {updateChecking ? <LoaderCircle className="button-icon spin" aria-hidden="true" /> : <Download className="button-icon" aria-hidden="true" />}
            {updateChecking ? '检查中' : updateInstalling ? '更新中' : '检查更新'}
          </button>
          <button
            className="ghost-button"
            aria-expanded={diagnosticPanelOpen}
            aria-controls="diagnostic-panel"
            onClick={() => {
              if (!diagnosticBusy) setDiagnosticPanelOpen((current) => !current);
            }}
            disabled={diagnosticBusy}
          >
            <Bug className="button-icon" aria-hidden="true" />诊断
          </button>
          {activePage !== 'skills' ? <button className="ghost-button" onClick={() => {
            handleManualRefresh();
          }} disabled={exclusiveBusy}><RefreshCw className="button-icon" aria-hidden="true" />刷新</button> : null}
        </div>
      </header>

      {diagnosticPanelOpen ? (
        <DiagnosticPanel
          onClose={() => {
            if (!diagnosticBusy) setDiagnosticPanelOpen(false);
          }}
          onBusyChange={setDiagnosticBusy}
        />
      ) : null}

      {updateVisible && updateResult ? (
        <section className="update-banner" aria-label="发现新版本" aria-live="polite" aria-busy={updateInstalling}>
          <div>
            <p className="eyebrow">发现正式版本 v{updateResult.latestVersion}</p>
            <h2>ChatGPT Switch 可以更新了</h2>
            {updateResult.releaseNotes ? <p className="update-notes">{updateResult.releaseNotes}</p> : null}
          </div>
          <div className="update-actions">
            <button className="warm-button" onClick={() => void handleInstallUpdate()} disabled={exclusiveBusy}>
              <Download className="button-icon" aria-hidden="true" />
              {updateInstalling ? '正在下载并安装…' : '立即更新'}
            </button>
            <button
              className="icon-button"
                aria-label="关闭更新提示"
              title="关闭更新提示"
              disabled={updateInstalling}
              onClick={() => setDismissedUpdateVersion(updateResult.latestVersion)}
            >
              <X aria-hidden="true" />
            </button>
          </div>
        </section>
      ) : null}
      {updateNotice ? <p className="busy-banner" role="status" aria-live="polite">{updateNotice}</p> : null}
      {startupUpdateError ? <p className="error-banner" role="alert"><strong>更新：</strong><span>{startupUpdateError}</span></p> : null}
      {updateError ? (
        <section className="error-banner operation-error-banner">
          <span role="alert"><strong>更新：</strong>{updateError.message}</span>
          {updateError.operationId ? (
            <DiagnosticExportAction operationId={updateError.operationId} />
          ) : null}
        </section>
      ) : null}
      {closeGuardStatus === 'failed' ? (
        <p className="error-banner" role="alert">
          窗口保护初始化失败，运行态切换已禁用。请重新启动 ChatGPT Switch 后重试。
        </p>
      ) : null}
      {activePage !== 'skills' ? domainErrors.map(({ domain, message }) => (
        <p className="error-banner" role="alert" key={domain}><strong>{domain}：</strong><span>{message}</span></p>
      )) : null}
      {activePage !== 'skills' && error ? (
        <section className="error-banner operation-error-banner">
          <span role="alert">{error.message}</span>
          {error.operationId ? <DiagnosticExportAction operationId={error.operationId} /> : null}
        </section>
      ) : null}
      {closePending && !switchFlow ? (
        <p className="busy-banner shutdown-pending" role="status" aria-live="polite">
          <LoaderCircle className="spin" aria-hidden="true" />
          正在安全完成当前任务，完成后将自动退出
        </p>
      ) : null}
      {busy && !switchFlow ? <p className="busy-banner" role="status" aria-live="polite"><LoaderCircle className="spin" aria-hidden="true" />{busy}处理中</p> : null}
      {!switchFlow && !busy && runtimeRefreshPending ? (
        <p className="busy-banner" role="status" aria-live="polite">
          <LoaderCircle className="spin" aria-hidden="true" />正在确认当前运行态
        </p>
      ) : null}
      {activePage !== 'skills' && receipt ? <OperationResultPanel result={receipt} /> : null}
      {pendingConfirmation ? (
        <InlineConfirmation
          pending={pendingConfirmation}
          busy={
            exclusiveBusy
            || (backupLoading && ['restoreBackup', 'deleteBackup'].includes(pendingConfirmation.kind))
          }
          onCancel={cancelPendingConfirmation}
          onConfirm={() => void confirmPendingAction()}
        />
      ) : null}
      {activePage === 'runtime' ? (
        <section className="runtime-page">
          <header className="runtime-intro">
            <div className="runtime-intro-copy">
              <p className="eyebrow">LOCAL RUNTIME / CONTROL 01</p>
              <h1><span>ChatGPT</span><ArrowLeftRight aria-hidden="true" /><span>API Relay</span></h1>
              <p className="runtime-intro-lede">
                只切换请求端，不改写现有登录文件。Relay 可在未登录的全新 Codex Home 中独立启用；
                后续登录后仍可安全切回 Account，并保留会话数据库视图。
              </p>
            </div>
            <dl className="runtime-readout" aria-label="当前扫描摘要">
              <div><dt>AUTH</dt><dd>{runtimeStatus?.authMode ?? authStatusLabel(data.codexHome)}</dd></div>
              <div><dt>RUNTIMES</dt><dd>{runtimes ? runtimes.length : statusLabel(data.runtimes)}</dd></div>
              <div><dt>THREADS</dt><dd>{threadCount}</dd></div>
              <div><dt>JSONL</dt><dd>{jsonlCount}</dd></div>
            </dl>
          </header>

          {codexHome && (!officialAuthReady || !codexHome.configToml.exists) ? (
            <section className="home-setup-notice" aria-label="Account 尚未就绪">
              <LogIn aria-hidden="true" />
              <div>
                <p className="eyebrow">ACCOUNT SETUP</p>
                <h2>Account 需要官方登录；中转站仍可直接使用</h2>
                <p>未检测到完整的 Account 运行态。配置或切换 Relay 不会创建、覆盖 auth.json。</p>
              </div>
              <button className="ghost-button" onClick={handleManualRefresh} disabled={exclusiveBusy}>
                <RefreshCw className="button-icon" aria-hidden="true" />
                刷新
              </button>
            </section>
          ) : null}

          <section className="runtime-grid" aria-label="运行态">
            <RuntimeCard
              title="ChatGPT 账号态" kind="plus" description="官方登录不变，使用 OpenAI 请求端。"
              runtime={plusRuntime} runtimeStatus={runtimeStatus} baseUrlFallback="本机 ChatGPT 登录态"
              runtimeDomainStatus={data.runtimes.status} runtimeStatusDomainStatus={data.runtimeStatus.status}
              onPrimary={() => void handleImportPlus()} primaryAction="保存当前账号态"
              onSwitch={(trigger) => void handleSwitch('plus', '切换 ChatGPT 账号', trigger)}
              switchAction={isExactRuntime(runtimeStatus, 'plus') ? '当前为 ChatGPT 账号' : runtimeStatus?.activeRuntimeId === 'plus' ? '重新应用 ChatGPT 账号' : '切换到 ChatGPT 账号'}
              primaryDisabled={exclusiveBusy || !canImportAccount}
              primaryHelp={!canImportAccount && !exclusiveBusy ? '保存 Account 槽位需要现有官方登录与 config.toml。' : null}
              switchDisabled={exclusiveBusy || !canSwitchAccount || !plusRuntime || isExactRuntime(runtimeStatus, 'plus')}
              switchHelp={!plusRuntime
                ? '先在已登录状态保存 Account 槽位。'
                : !officialAuthReady
                  ? '切换到 Account 前需要先在 ChatGPT 完成官方登录。'
                  : runtimeGateHelp}
            />
            <RuntimeCard
              title="API 中转站态" kind="relay" description="Key 加密保存；激活时明文投影到当前请求配置。"
              runtime={relayRuntime} runtimeStatus={runtimeStatus} baseUrlFallback="尚未配置"
              runtimeDomainStatus={data.runtimes.status} runtimeStatusDomainStatus={data.runtimeStatus.status}
              onPrimary={() => { setRelaySubmitError(null); setRelayEditorOpen(true); }} primaryAction="配置中转站"
              onSwitch={(trigger) => void handleSwitch('relay', '切换中转站', trigger)}
              switchAction={isExactRuntime(runtimeStatus, 'relay') ? '当前为中转站' : runtimeStatus?.activeRuntimeId === 'relay' ? '重新应用中转站' : '切换到中转站'}
              primaryDisabled={exclusiveBusy || !canConfigureRelay}
              primaryHelp={!canConfigureRelay && !exclusiveBusy ? '运行态存储尚未就绪。' : null}
              switchDisabled={exclusiveBusy || !canSwitchRelay || !relayRuntime || isExactRuntime(runtimeStatus, 'relay')}
              switchHelp={!relayRuntime ? '先配置中转站。' : runtimeGateHelp}
            />
          </section>

          {mobileContinuity?.noticePending ? (
            <section className="home-setup-notice mobile-continuity-notice" aria-label="Relay 会话视图已启用">
              <MessagesSquare aria-hidden="true" />
              <div>
                <p className="eyebrow">CANONICAL VIEW</p>
                <h2>Relay 与 Account 共用 canonical 会话正文</h2>
                <p>切回 Account 时只提升 SQLite 视图，不复制或改写 JSONL。</p>
              </div>
              <button
                className="ghost-button"
                onClick={() => void handleAcknowledgeMobileContinuity()}
                disabled={exclusiveBusy}
              >
                知道了
              </button>
            </section>
          ) : null}

          {relayEditorOpen ? (
            <RelayRuntimeDialog
              runtime={relayRuntime} fallbackModel={plusRuntime?.model ?? ''} busy={exclusiveBusy}
              submitError={relaySubmitError}
              onCancel={() => { setRelaySubmitError(null); setRelayEditorOpen(false); }} onSave={handleSaveRelay}
            />
          ) : null}

          <section className="runtime-operations" aria-label="数据与恢复">
            <SessionStoragePanel
              state={data.sessionStorage}
              scanning={storageScanning}
              onScan={() => void handleSessionStorageScan()}
            />
            <aside className="detail-panel session-panel mobile-continuity-panel" aria-label="Relay 会话视图">
              <div className="card-title-row">
                <MessagesSquare className="section-icon" aria-hidden="true" />
                <div><p className="eyebrow">CANONICAL VIEW</p><h2>Relay 会话视图</h2></div>
              </div>
              {mobileContinuity ? (
                <>
                  <div className="continuity-stats">
                    <strong>{mobileContinuity.queued + mobileContinuity.publishing}<span>待加入视图</span></strong>
                    <strong>{mobileContinuity.remotePublished}<span>Account 视图</span></strong>
                    <strong>{mobileContinuity.partial}<span>兼容状态</span></strong>
                    <strong>{mobileContinuity.conflict + mobileContinuity.needsManual}<span>需处理</span></strong>
                  </div>
                  <p className="continuity-copy">
                    {mobileContinuity.enabled
                      ? '已开启：显式加入 Account 视图时只更新数据库引用。'
                      : '已关闭：保留已有状态，不会恢复旧 provider 正文复制。'}
                  </p>
                  <button
                    className="ghost-button full"
                    onClick={() => void handleMobileContinuityToggle()}
                    disabled={exclusiveBusy}
                  >
                    {mobileContinuity.enabled ? '关闭显式加入' : '开启显式加入'}
                  </button>
                </>
              ) : <p className="continuity-copy">连续性状态读取中…</p>}
            </aside>
            <aside className="detail-panel session-panel" aria-label="会话合并与修复">
              <div className="card-title-row"><Database className="section-icon" aria-hidden="true" /><div><p className="eyebrow">CANONICAL MERGE</p><h2>会话合并与修复</h2></div></div>
              <div className="sync-stats"><strong>{threadCount}<span>threads</span></strong><strong>{jsonlCount}<span>JSONL</span></strong></div>
              <p className="continuity-copy">{sessionStorage?.migrationRequired === false ? '仅合并缺失、相同或完整延续的会话。' : '完成 v0.3 前台迁移后可用；旧完全同步已停用。'}</p>
              <button className="primary-button full" onClick={handleSyncSessions} disabled={exclusiveBusy || !canSync}><RefreshCw className="button-icon" aria-hidden="true" />会话合并与修复</button>
            </aside>
            <SafetyPanel data={data} sessionsStale={sessionsStale} backupsStale={backupsStale} />
            <BackupRecoveryPanel
              state={data.backups}
              storage={data.backupStorage}
              stale={backupsStale}
              disabled={exclusiveBusy || backupLoading || !canRestoreBackup}
              loadDisabled={exclusiveBusy || backupLoading}
              createDisabled={exclusiveBusy || backupLoading}
              cleanupDisabled={exclusiveBusy || backupLoading}
              creating={busy === '创建完整备份'}
              cleanupFlow={checkpointCleanupFlow}
              onLoad={handleLoadBackups}
              onCreate={() => void handleCreateFullBackup()}
              onCleanup={() => void handleCleanupAutomaticCheckpoints()}
              onRestore={handleRestoreBackup}
              onDelete={handleDeleteBackup}
            />
            <OperationHistoryPanel state={data.operations} />
          </section>
        </section>
      ) : activePage === 'sessions' && sessionsStale ? (
        <section className="domain-loading" aria-live="polite">
          <LoaderCircle className="spin" aria-hidden="true" />
          <p className="eyebrow">SESSION INDEX</p>
          <h2>正在刷新 ChatGPT 会话</h2>
        </section>
      ) : activePage === 'sessions' && managedSessions ? (
        <SessionManagementPage
          inventory={managedSessions} busy={exclusiveBusy}
          syncDisabled={!canSync} mutationDisabled={!canMutateSessions}
          onSync={() => void handleSyncSessions()}
          onRestoreVisible={handleRestoreSessionsVisible}
          mobileContinuity={mobileContinuity}
          onPublishMobile={(threadId) => handlePublishMobileContinuitySession(threadId)}
          mobilePublishDisabled={!isExactRuntime(runtimeStatus, 'plus')}
        />
      ) : activePage === 'sessions' ? <DomainPlaceholder state={data.managedSessions} /> : null}

      <SessionStorageManagementPage
        active={activePage === 'storage'}
        initialReport={sessionStorage ?? null}
        onReportChange={(report) => {
          setData((current) => ({
            ...current,
            sessionStorage: { status: 'ready', data: report },
          }));
        }}
        onBusyChange={handleChildBusyChange}
      />

      <SkillsManagementPage
        active={activePage === 'skills'}
        busy={exclusiveBusy}
        onBusyChange={handleChildBusyChange}
        ensureCodexClosed={ensureChatGptClosed}
      />
    </main>
    {switchFlow ? (
      <RuntimeSwitchProgressPanel
        flow={switchFlow}
        closeDisabled={runtimeRefreshPending}
        closePending={closePending}
        onClose={closeSwitchTask}
        onRetryLaunch={() => void handleRetryChatGptLaunch()}
      />
    ) : null}
    {continuityCloseChoice === 'pending' ? (
      <section className="close-choice-overlay" role="dialog" aria-modal="true" aria-label="会话正在同步">
        <div className="close-choice-card">
          <p className="eyebrow">REMOTE PUBLICATION</p>
          <h2>会话正在同步</h2>
          <p>继续等待可完成当前发布；仍然退出会在当前原子步骤结束后保存队列并真正退出。</p>
          <div className="relay-switch-choice-actions">
            <button
              className="primary-button"
              onClick={() => {
                continuityCloseChoiceRef.current = null;
                setContinuityCloseChoice(null);
                setClosePending(false);
              }}
            >
              继续等待
            </button>
            <button
              className="ghost-button"
              onClick={() => {
                continuityCloseChoiceRef.current = 'exit';
                setContinuityCloseChoice('exit');
                void attemptAppExit();
              }}
            >
              仍然退出
            </button>
          </div>
        </div>
      </section>
    ) : null}
    </>
  );
}

function InlineConfirmation({
  pending,
  busy,
  onCancel,
  onConfirm,
}: {
  pending: PendingConfirmation;
  busy: boolean;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const headingRef = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    const reducedMotion = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
    headingRef.current?.scrollIntoView?.({ block: 'center', behavior: reducedMotion ? 'auto' : 'smooth' });
    headingRef.current?.focus();
  }, [pending.kind]);

  let title = '确认操作';
  let detail = '';
  let metrics: string[] = [];
  let confirmLabel = '继续';
  const destructive = pending.kind === 'deleteBackup';

  if (pending.kind === 'importAccount') {
    title = '覆盖已保存的 ChatGPT 账号态';
    detail = '当前账号态会先归档，再写入新的加密快照。';
    confirmLabel = '确认覆盖';
  } else if (pending.kind === 'syncSessions') {
    title = '会话合并与修复';
    detail = '确认后会安全关闭 ChatGPT，只处理缺失、相同或完整延续的会话，并修复 canonical 数据库视图。';
    metrics = ['不生成 provider 正文副本', '冲突默认不覆盖'];
    confirmLabel = '开始合并与修复';
  } else if (pending.kind === 'restoreBackup') {
    title = '恢复已验证备份';
    detail = `来源：${pending.backup.sourceRoot}`;
    metrics = [`快照：${pending.backup.backupDir}`];
    confirmLabel = '开始恢复';
  } else {
    title = '删除此恢复点';
    detail = '这会永久删除备份本身；删除后无法从 ChatGPT Switch 恢复。当前 ChatGPT 数据不会被删除。';
    metrics = [`路径：${pending.backup.backupDir}`];
    confirmLabel = '确认删除恢复点';
  }

  return (
    <section
      className={`inline-confirmation ${destructive ? 'danger-confirmation' : 'warning-confirmation'}`}
      aria-label={title}
    >
      <CircleAlert className="section-icon" aria-hidden="true" />
      <div className="confirmation-copy">
        <p className="eyebrow">REVIEW ACTION</p>
        <h2 ref={headingRef} tabIndex={-1}>{title}</h2>
        <p>{detail}</p>
        {metrics.length > 0 ? <div className="confirmation-metrics">{metrics.map((metric) => <span key={metric}>{metric}</span>)}</div> : null}
      </div>
      <div className="confirmation-actions">
        <button className="ghost-button" onClick={onCancel} disabled={busy}><X className="button-icon" aria-hidden="true" />取消</button>
        <button className={destructive ? 'ghost-button danger' : 'warm-button'} onClick={onConfirm} disabled={busy}>
          {destructive
            ? <Trash2 className="button-icon" aria-hidden="true" />
            : <Check className="button-icon" aria-hidden="true" />}
          {confirmLabel}
        </button>
      </div>
    </section>
  );
}

function RuntimeCard({
  title, kind, description, runtime, runtimeStatus, baseUrlFallback, primaryAction, switchAction,
  runtimeDomainStatus, runtimeStatusDomainStatus, onPrimary, onSwitch,
  primaryDisabled, switchDisabled, primaryHelp, switchHelp,
}: {
  title: string; kind: RuntimeKind; description: string; runtime: RuntimeMetadata | null;
  runtimeStatus: RuntimeStatus | null; baseUrlFallback: string; primaryAction: string; switchAction: string;
  runtimeDomainStatus: DomainState<RuntimeMetadata[]>['status'];
  runtimeStatusDomainStatus: DomainState<RuntimeStatus>['status'];
  onPrimary: () => void; onSwitch: (trigger?: HTMLElement) => void;
  primaryDisabled: boolean; switchDisabled: boolean;
  primaryHelp?: string | null; switchHelp?: string | null;
}) {
  const savedState = runtimeDomainStatus === 'ready'
    ? runtime ? '已保存' : '未保存'
    : domainStatusText(runtimeDomainStatus);
  const activeState = runtimeStatusDomainStatus !== 'ready'
    ? domainStatusText(runtimeStatusDomainStatus)
    : !runtimeStatus ? '未检测到' : runtimeStatus.activeRuntimeId === kind
    ? runtimeStatus.confidence === 'exact' ? '当前运行' : '模式匹配'
    : '非当前';
  const runtimeDetailUnavailable = runtimeDomainStatus !== 'ready';
  const RuntimeIcon = kind === 'plus' ? UserRound : KeyRound;
  return (
    <article className={`runtime-card ${runtime?.kind ?? 'empty'}`} aria-label={title} tabIndex={-1}>
      <div className="runtime-card-head">
        <div className="card-title-row"><RuntimeIcon className="section-icon" aria-hidden="true" /><div><p className="eyebrow">{kind === 'plus' ? 'ACCOUNT' : 'RELAY'}</p><h2>{title}</h2></div></div>
        <span className="runtime-card-index" aria-hidden="true">{kind === 'plus' ? '01' : '02'}</span>
      </div>
      <p className="runtime-description">{description}</p>
      <div className="runtime-state-grid">
        <span className={stateClass(savedState)}>{savedState}</span>
        <span className={stateClass(activeState)}>{activeState}</span>
      </div>
      <dl className="meta-list">
        <div><dt>Base URL</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.baseUrl ?? baseUrlFallback}</dd></div>
        <div><dt>模型</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.model ?? '跟随当前 ChatGPT 配置'}</dd></div>
      </dl>
      <div className="runtime-actions">
        <button className="ghost-button inline" onClick={onPrimary} disabled={primaryDisabled}>{kind === 'plus' ? <Save className="button-icon" aria-hidden="true" /> : <Settings2 className="button-icon" aria-hidden="true" />}{primaryAction}</button>
        <button className="switch-button" onClick={(event) => onSwitch(event?.currentTarget)} disabled={switchDisabled}><ArrowLeftRight className="button-icon" aria-hidden="true" />{switchAction}</button>
      </div>
      {primaryDisabled && primaryHelp ? <p className="runtime-action-help">{primaryHelp}</p> : null}
      {switchDisabled && switchHelp ? <p className="runtime-action-help">{switchHelp}</p> : null}
      <p className="runtime-switch-note"><Power aria-hidden="true" />任务执行器会安全关闭，并在成功后自动打开 ChatGPT</p>
    </article>
  );
}

function SafetyPanel({
  data,
  sessionsStale,
  backupsStale,
}: {
  data: DashboardData;
  sessionsStale: boolean;
  backupsStale: boolean;
}) {
  const home = readyData(data.codexHome);
  const status = readyData(data.runtimeStatus);
  const backups = readyData(data.backups);
  const latestBackup = backups?.[0];
  const storageReport = readyData(data.sessionStorage);
  const homeFilesReady = Boolean(home?.authJson.exists && home.configToml.exists && home.stateDb.exists);
  const homeState = data.codexHome.status === 'ready' ? homeFilesReady ? '完整' : '缺失' : statusLabel(data.codexHome);
  const backupState = backupsStale
    ? '待加载'
    : data.backups.status === 'ready'
    ? latestBackup?.verified ? '已验证' : '无已验证备份'
    : statusLabel(data.backups);
  return (
    <aside className="detail-panel safety-panel" aria-label="安全检查">
      <div className="card-title-row"><ShieldCheck className="section-icon" aria-hidden="true" /><div><p className="eyebrow">GUARDRAILS</p><h2>切换保护</h2></div></div>
      <SafetyLine
        state={data.codexHome.status === 'loading' ? 'pending' : data.codexHome.status === 'error' ? 'error' : homeFilesReady ? 'ok' : 'warning'}
        label={`ChatGPT 数据文件：${homeState}`}
      />
      <SafetyLine
        state={data.runtimeStatus.status === 'loading' ? 'pending' : data.runtimeStatus.status === 'error' ? 'error' : status?.confidence === 'exact' ? 'ok' : 'warning'}
        label={`运行态检测：${status?.confidence ?? statusLabel(data.runtimeStatus)}`}
      />
      <SafetyLine
        state={backupsStale || data.backups.status === 'loading' ? 'pending' : data.backups.status === 'error' ? 'error' : latestBackup?.verified ? 'ok' : 'warning'}
        label={`最近备份：${backupState}`}
      />
      <SafetyLine
        state={sessionsStale || data.sessions.status === 'loading' ? 'pending' : data.sessions.status === 'error' ? 'error' : 'ok'}
        label={`会话索引：${sessionsStale ? '待刷新' : statusLabel(data.sessions)}`}
      />
      <SafetyLine
        state={data.sessionStorage.status === 'loading'
          ? 'pending'
          : data.sessionStorage.status === 'error'
          ? 'error'
          : storageReport?.status === 'reviewRequired'
          ? 'warning'
          : storageReport
          ? 'ok'
          : 'warning'}
        label={`会话存储：${storageReport ? storageScanStatusLabel(storageReport.status) : data.sessionStorage.status === 'ready' ? '尚未扫描' : statusLabel(data.sessionStorage)}`}
      />
    </aside>
  );
}

function SessionStoragePanel({
  state,
  scanning,
  onScan,
}: {
  state: DomainState<ShadowScanReport | null>;
  scanning: boolean;
  onScan: () => void;
}) {
  const report = readyData(state);
  const issues = report?.issues.reduce((total, issue) => total + issue.count, 0) ?? 0;
  return (
    <aside className="detail-panel session-panel" aria-label="会话存储状态">
      <div className="card-title-row">
        <Database className="section-icon" aria-hidden="true" />
        <div><p className="eyebrow">SHADOW SCAN</p><h2>会话存储</h2></div>
      </div>
      {state.status === 'loading' ? (
        <p className="continuity-copy">正在读取最近一次扫描结果…</p>
      ) : state.status === 'error' ? (
        <p className="continuity-copy">{state.error}</p>
      ) : report ? (
        <>
          <div className="sync-stats">
            <strong>{report.summary.logicalSessionCount}<span>逻辑会话</span></strong>
            <strong>{report.summary.canonicalCandidateCount}<span>候选主版本</span></strong>
            <strong>{report.summary.highConfidenceCopyCount}<span>高置信副本</span></strong>
            <strong>{report.summary.conflictSessionCount}<span>需复核</span></strong>
          </div>
          <p className="continuity-copy">
            {storageScanStatusLabel(report.status)} · 潜在可释放 {formatBytes(report.summary.potentialReclaimBytes)}
            {issues > 0 ? ` · ${issues} 项检查提示` : ''}
          </p>
          <p className="continuity-copy">
            回合来源已解析 {report.summary.resolvedTurnProvenanceCount}/{report.summary.turnContextCount}
            {report.summary.historicalUnknownTurnCount > 0
              ? ` · ${report.summary.historicalUnknownTurnCount} 个迁移前回合来源未知`
              : ''}
            {report.summary.incompleteTurnProvenanceCount > 0
              ? ` · ${report.summary.incompleteTurnProvenanceCount} 个来源记录不完整`
              : ''}
          </p>
          <p className="runtime-switch-note">
            <ShieldCheck aria-hidden="true" />
            在线仅扫描，不删除；最近扫描 {formatTime(report.generatedAtMs)}
          </p>
        </>
      ) : (
        <p className="continuity-copy">尚未执行 v0.3 Shadow 扫描；扫描只生成脱敏分类报告。</p>
      )}
      <button className="ghost-button full" onClick={onScan} disabled={scanning}>
        {scanning
          ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
          : <RefreshCw className="button-icon" aria-hidden="true" />}
        {scanning ? '扫描中' : '扫描会话存储'}
      </button>
    </aside>
  );
}

function storageScanStatusLabel(status: ShadowScanReport['status']) {
  const labels: Record<ShadowScanReport['status'], string> = {
    noSessions: '未检测到会话',
    canonicalReady: 'Canonical 存储已就绪',
    migrationAvailable: '检测到旧版存储，建议迁移',
    reviewRequired: '发现冲突或异常，需先复核',
  };
  return labels[status];
}

function BackupRecoveryPanel({
  state,
  storage,
  stale,
  disabled,
  loadDisabled,
  createDisabled,
  cleanupDisabled,
  creating,
  cleanupFlow,
  onLoad,
  onCreate,
  onCleanup,
  onRestore,
  onDelete,
}: {
  state: DomainState<BackupSummary[]>;
  storage: DomainState<CheckpointStorageStatus>;
  stale: boolean;
  disabled: boolean;
  loadDisabled: boolean;
  createDisabled: boolean;
  cleanupDisabled: boolean;
  creating: boolean;
  cleanupFlow: CheckpointCleanupFlow | null;
  onLoad: () => void;
  onCreate: () => void;
  onCleanup: () => void;
  onRestore: (backup: BackupSummary) => void;
  onDelete: (backup: BackupSummary) => void;
}) {
  const verifiedBackups = state.status === 'ready' ? state.data.filter((item) => item.verified) : [];
  const storageStatus = storage.status === 'ready' ? storage.data : null;
  const cleanupRunning = cleanupFlow?.status === 'running';
  return (
    <aside className="detail-panel backup-panel" aria-label="备份与恢复">
      <div className="card-title-row"><RotateCcw className="section-icon" aria-hidden="true" /><div><p className="eyebrow">SNAPSHOTS</p><h2>备份与恢复</h2></div></div>
      <p className="runtime-description">已验证完整备份会持续保留，可逐份恢复或删除</p>
      <button className="primary-button full backup-create-button" onClick={onCreate} disabled={createDisabled}>
        {creating
          ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
          : <HardDriveDownload className="button-icon" aria-hidden="true" />}
        {creating ? '正在创建完整备份' : '创建完整备份'}
      </button>
      <p className="runtime-switch-note"><Power aria-hidden="true" />创建时会先关闭 ChatGPT，并保留当前 Home 与共享池两份快照</p>
      {!stale ? (
        <section className="checkpoint-storage" aria-label="自动检查点空间">
          <div className="checkpoint-storage-heading">
            <div>
              <p className="eyebrow">TRANSIENT CHECKPOINTS</p>
              <h3>自动检查点空间</h3>
            </div>
            {storageStatus ? <strong>{formatBytes(storageStatus.reclaimableBytes)} 可释放</strong> : null}
          </div>
          {storage.status === 'loading' ? (
            <p className="checkpoint-storage-status" role="status">
              <LoaderCircle className="spin" aria-hidden="true" />正在核对操作终态
            </p>
          ) : storage.status === 'error' ? (
            <p className="checkpoint-storage-status" role="alert">{storage.error}</p>
          ) : (
            <>
              <dl className="checkpoint-storage-metrics">
                <div><dt>目录占用</dt><dd>{formatBytes(storage.data.totalBytes)}</dd></div>
                <div><dt>可证明回收</dt><dd>{storage.data.reclaimableCount}</dd></div>
                <div><dt>安全保留</dt><dd>{storage.data.retainedCount}</dd></div>
              </dl>
              <p className="checkpoint-storage-copy">
                请求端切换不创建检查点。会话同步等写操作只创建覆盖实际写集的轻量临时点；
                成功或可证明的写入前失败会自动释放，写入后失败、回滚失败、孤儿和证据不足项
                继续保留。完整备份由你通过“删除恢复点”显式管理。
              </p>
              <button
                className="ghost-button inline checkpoint-cleanup-button"
                onClick={onCleanup}
                disabled={
                  cleanupDisabled
                  || cleanupRunning
                  || storage.data.reclaimableCount === 0
                }
              >
                {cleanupRunning
                  ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
                  : <Trash2 className="button-icon" aria-hidden="true" />}
                {cleanupRunning
                  ? '正在安全释放'
                  : storage.data.reclaimableCount > 0
                    ? `安全释放 ${formatBytes(storage.data.reclaimableBytes)}`
                    : '没有可安全释放的检查点'}
              </button>
              {storage.data.warnings.length > 0 ? (
                <div className="checkpoint-warnings" aria-label="保留说明">
                  {storage.data.warnings.map((warning) => (
                    <p className="checkpoint-warning" key={warning}>{warning}</p>
                  ))}
                </div>
              ) : null}
              {storage.data.lastCleanup ? (
                <p className="checkpoint-last-cleanup">
                  最近清理：计划 {storage.data.lastCleanup.attemptedCount} 项，
                  失败 {storage.data.lastCleanup.failedCount} 项，
                  释放 {formatBytes(storage.data.lastCleanup.reclaimedBytes)}，
                  保留 {storage.data.lastCleanup.retainedCount} 项
                </p>
              ) : null}
            </>
          )}
          {cleanupFlow ? <CheckpointCleanupProgress flow={cleanupFlow} /> : null}
        </section>
      ) : null}
      {stale ? (
        <div className="backup-load-state">
          <p>备份校验按需执行，避免与 ChatGPT 抢占磁盘。</p>
          <button className="ghost-button inline" onClick={onLoad} disabled={loadDisabled}>
            <RefreshCw className="button-icon" aria-hidden="true" />加载备份
          </button>
        </div>
      ) : state.status === 'error' ? <div className="backup-load-state" role="alert"><p>{state.error}</p><button className="ghost-button inline" onClick={onLoad} disabled={loadDisabled}><RefreshCw className="button-icon" aria-hidden="true" />重试</button></div>
        : state.status === 'loading' ? <p className="empty-state">备份列表扫描中...</p>
        : verifiedBackups.length > 0 ? (
          <div
            className="backup-list"
            role="region"
            aria-label={`已验证完整备份，共 ${verifiedBackups.length} 份`}
            tabIndex={0}
          >
        {verifiedBackups.map((backup) => <article className="backup-entry" key={backup.backupDir}>
          <dl className="compact-meta">
            <div><dt>原因</dt><dd>{backup.reason}</dd></div>
            <div><dt>时间</dt><dd>{formatTime(backup.createdAtMs)}</dd></div>
            <div><dt>文件</dt><dd>{backup.fileCount}</dd></div>
          </dl>
          <p className="backup-path" title={backup.sourceRoot}>来源：{backup.sourceRoot}</p>
          <p className="backup-path" title={backup.backupDir}>{backup.backupDir}</p>
          <div className="backup-entry-actions">
            <button
              className="warm-button"
              aria-label={`恢复此备份，${formatTime(backup.createdAtMs)}，来源 ${backup.sourceRoot}`}
              onClick={() => onRestore(backup)}
              disabled={disabled}
            ><RotateCcw className="button-icon" aria-hidden="true" />恢复此备份</button>
            <button
              className="ghost-button danger"
              aria-label={`删除恢复点，${formatTime(backup.createdAtMs)}，路径 ${backup.backupDir}`}
              onClick={() => onDelete(backup)}
              disabled={disabled}
            ><Trash2 className="button-icon" aria-hidden="true" />删除恢复点</button>
          </div>
        </article>)}
          </div>
        ) : <p className="empty-state">没有可恢复的已验证备份。</p>}
    </aside>
  );
}

function CheckpointCleanupProgress({ flow }: { flow: CheckpointCleanupFlow }) {
  const complete = flow.status === 'succeeded';
  const partial = flow.status === 'partial';
  const completeWithNotes = complete && Boolean(flow.receipt?.warnings.length);
  return (
    <section
      className={`checkpoint-cleanup-flow ${flow.status}`}
      role="region"
      aria-label="自动检查点清理进度"
      aria-live="polite"
      aria-busy={flow.status === 'running'}
    >
      <div className="checkpoint-cleanup-flow-title">
        {flow.status === 'running'
          ? <LoaderCircle className="spin" aria-hidden="true" />
          : complete
            ? <Check aria-hidden="true" />
            : <CircleAlert aria-hidden="true" />}
        <strong>
          {flow.status === 'running'
            ? '正在执行安全清理任务'
            : complete
              ? completeWithNotes
                ? '安全清理任务已完成（有保留说明）'
                : '安全清理任务已完成'
              : partial
                ? '安全清理任务部分完成'
                : '安全清理任务未完成'}
        </strong>
      </div>
      {flow.status === 'running' ? (
        <p role="status">
          正在核对持久化终态与完整检查点证据；只会删除符合安全合同的目录。
        </p>
      ) : null}
      {flow.receipt ? (
        <p>
          计划 {flow.receipt.attemptedCount} 项，失败 {flow.receipt.failedCount} 项，
          已释放 {formatBytes(flow.receipt.reclaimedBytes)}，
          {flow.receipt.retainedCount} 项因恢复职责或证据不足继续保留。
        </p>
      ) : null}
      {flow.error ? (
        <>
          <p role="alert">{flow.error}</p>
          {flow.operationId ? <DiagnosticExportAction operationId={flow.operationId} /> : null}
        </>
      ) : null}
    </section>
  );
}

function OperationHistoryPanel({ state }: { state: DomainState<OperationRecord[]> }) {
  return (
    <aside className="detail-panel operation-history-panel" aria-label="操作历史">
      <div className="card-title-row"><History className="section-icon" aria-hidden="true" /><div><p className="eyebrow">LOCAL AUDIT</p><h2>操作历史</h2></div></div>
      {state.status === 'error' ? <p className="empty-state" role="alert">{state.error}</p>
        : state.status === 'loading' ? <p className="empty-state">操作历史加载中...</p>
        : state.data.length === 0 ? <p className="empty-state">暂无操作记录。</p>
        : <div className="operation-history-list">
          {state.data.slice(0, 10).map((record) => <article className={`operation-history-row ${record.status}`} key={`${record.operationId}-${record.completedAtMs}`}>
            <div><strong>{operationActionLabel(record.action)}</strong><span>{operationStatusLabel(record.status)}</span></div>
            <code>{record.operationId}</code>
            <time>{formatTime(record.completedAtMs)}</time>
            {record.backupDirs.map((path) => <p className="backup-path" title={path} key={path}>{path}</p>)}
          </article>)}
        </div>}
    </aside>
  );
}

function SafetyLine({
  state,
  label,
}: {
  state: 'ok' | 'pending' | 'warning' | 'error';
  label: string;
}) {
  const Icon = state === 'ok' ? Check : state === 'pending' ? LoaderCircle : CircleAlert;
  return <div className={`safety-line ${state}`}><Icon className={state === 'pending' ? 'spin' : ''} aria-hidden="true" /><strong>{label}</strong></div>;
}

function DomainPlaceholder<T>({ state }: { state: DomainState<T> }) {
  return <section className="domain-loading">{state.status === 'error' ? <CircleAlert aria-hidden="true" /> : <LoaderCircle className="spin" aria-hidden="true" />}<h2>{state.status === 'error' ? state.error : '会话数据扫描中'}</h2></section>;
}

function readyData<T>(state: DomainState<T>): T | null {
  return state.status === 'ready' ? state.data : null;
}

function isExactRuntime(status: RuntimeStatus | null, runtimeId: RuntimeKind) {
  return status?.activeRuntimeId === runtimeId && status.confidence === 'exact';
}

function lastRuntimeWorkPhase(events: RuntimeSwitchProgress[]) {
  return [...events]
    .reverse()
    .find((event) => !['rollingBack', 'complete', 'failed'].includes(event.phase))
    ?.phase;
}

function statusLabel<T>(state: DomainState<T>) {
  if (state.status === 'loading') return '扫描中';
  if (state.status === 'error') return '不可用';
  return '就绪';
}

function domainStatusText(status: DomainState<unknown>['status']) {
  if (status === 'loading') return '扫描中';
  if (status === 'error') return '不可用';
  return '就绪';
}

function authStatusLabel(state: DashboardData['codexHome']) {
  if (state.status !== 'ready') return statusLabel(state);
  if (!state.data.authJson.exists) return '缺失';
  return state.data.authSummary?.authMode ?? '未检测到';
}

function stateClass(label: string) {
  if (['已保存', '当前运行', '已验证'].includes(label)) return 'state-ok';
  if (['未保存', '不可用', '缺失'].includes(label)) return 'state-missing';
  return 'state-neutral';
}

function operationActionLabel(action: OperationRecord['action']) {
  const labels: Record<OperationRecord['action'], string> = {
    importAccount: '保存账号态', saveRelay: '保存中转站', verifyRelay: '验证中转站',
    switchRuntime: '切换运行态', incrementalSync: '增量会话同步',
    syncSessions: '会话合并与修复', deleteSessions: '删除会话',
    restoreVisibility: '恢复会话可见', restoreBackup: '恢复备份', createBackup: '创建完整备份',
    deleteBackup: '删除恢复点', cleanupCheckpoints: '清理自动检查点',
    installSkill: '安装技能', configureSkill: '配置技能',
  };
  return labels[action];
}

function operationStatusLabel(status: OperationRecord['status']) {
  const labels: Record<OperationRecord['status'], string> = {
    succeeded: '成功', failed: '失败', rolledBack: '已回滚', rollbackFailed: '回滚失败',
  };
  return labels[status];
}

function dashboardErrors(data: DashboardData) {
  const domains: Array<[string, DomainState<unknown>]> = [
    ['ChatGPT 本机数据', data.codexHome], ['会话扫描', data.sessions], ['会话管理', data.managedSessions],
    ['会话存储', data.sessionStorage],
    ['运行态列表', data.runtimes], ['当前运行态', data.runtimeStatus], ['备份列表', data.backups],
    ['操作历史', data.operations],
  ];
  return domains.flatMap(([domain, state]) => state.status === 'error' ? [{ domain, message: state.error }] : []);
}

function syncReceipt(
  result: SessionSyncResult,
  progressEvents: SessionSyncProgress[] = [],
): OperationView {
  const backups = result.backups ?? [];
  const cleanupComplete = result.checkpointCleanup
    && result.checkpointCleanup.failedCount === 0
    && result.checkpointCleanup.retainedCount === 0;
  const sessionNetBytes = result.persistentSessionBytesAdded
    - result.persistentSessionBytesReclaimed;
  return {
    label: '会话合并与修复完成',
    operationId: result.operationId,
    backupCount: cleanupComplete ? 0 : backups.length,
    backupPaths: cleanupComplete ? [] : backups.map((backup) => backup.backupDir),
    rolledBack: result.rolledBack,
    warnings: result.warnings,
    metrics: [
      `新增线程：${result.insertedThreads}`,
      `复制 JSONL：${result.copiedSessionFiles}`,
      `跳过缺失正文：${result.skippedMissingSessionFiles}`,
      `会话新增占用：${formatBytes(result.persistentSessionBytesAdded)}`,
      `旧槽位回收：${formatBytes(result.persistentSessionBytesReclaimed)}`,
      `会话净变化：${sessionNetBytes === 0
        ? '0 B'
        : `${sessionNetBytes > 0 ? '+' : '−'}${formatBytes(Math.abs(sessionNetBytes))}`}`,
      ...(result.checkpointCleanup
        ? [`临时检查点已释放：${formatBytes(result.checkpointCleanup.reclaimedBytes)}`]
        : []),
      ...sessionSyncTimingMetrics(progressEvents),
      `ChatGPT：${chatGptLaunchLabel(result.chatgptLaunch.status)}`,
    ],
  };
}

function sessionSyncTimingMetrics(events: SessionSyncProgress[]) {
  const labels: Partial<Record<SessionSyncProgress['phase'], string>> = {
    preparing: '准备',
    closingApp: '关闭 ChatGPT',
    backingUp: '创建安全检查点',
    reconciling: '对账活跃会话',
    recordingResult: '记录与清理',
    launchingApp: '重新打开 ChatGPT',
  };
  const timings = events.flatMap((event, index) => {
    const label = labels[event.phase];
    const next = events[index + 1];
    if (!label || !next) return [];
    const duration = Math.max(0, next.timestampMs - event.timestampMs);
    return [`耗时·${label}：${(duration / 1000).toFixed(1)}s`];
  });
  const first = events[0];
  const terminal = [...events].reverse().find((event) => (
    event.phase === 'complete' || event.phase === 'failed'
  ));
  if (first && terminal) {
    timings.push(`合并与修复总耗时：${(Math.max(0, terminal.timestampMs - first.timestampMs) / 1000).toFixed(1)}s`);
  }
  return timings;
}

function sessionSyncProgressLabel(event: SessionSyncProgress) {
  const labels: Record<SessionSyncProgress['phase'], string> = {
    preparing: '合并与修复：准备',
    closingApp: '合并与修复：关闭 ChatGPT',
    backingUp: '合并与修复：创建安全检查点',
    reconciling: '合并与修复：对账会话',
    recordingResult: '合并与修复：记录结果',
    launchingApp: '合并与修复：重新打开 ChatGPT',
    complete: '合并与修复：完成',
    failed: '合并与修复：失败',
  };
  return labels[event.phase];
}

function chatGptLaunchLabel(status: SessionSyncResult['chatgptLaunch']['status']) {
  if (status === 'launched') return '已重新打开';
  if (status === 'alreadyRunning') return '已在运行';
  if (status === 'failed') return '打开失败';
  if (status === 'blocked') return '为安全保持关闭';
  return '未请求';
}

function mutationReceipt(label: string) {
  return (result: SessionMutationResult): OperationView => {
    const cleanupComplete = result.checkpointCleanup
      && result.checkpointCleanup.failedCount === 0
      && result.checkpointCleanup.reclaimedCount === result.backups.length;
    return {
      label,
      operationId: result.operationId,
      backupCount: cleanupComplete ? undefined : result.backups.length,
      rolledBack: result.rolledBack,
      backupPaths: cleanupComplete ? [] : result.backups.map((backup) => backup.backupDir),
      warnings: result.warnings,
      metrics: [
        `删除线程：${result.deletedThreads}`,
        `删除 JSONL：${result.deletedSessionFiles}`,
        `恢复线程：${result.restoredThreads}`,
        ...(result.checkpointCleanup
          ? [`临时检查点已释放：${formatBytes(result.checkpointCleanup.reclaimedBytes)}`]
          : []),
      ],
    };
  };
}

function formatTime(value: number | null) {
  return value ? new Date(value).toLocaleString('zh-CN', { hour12: false }) : '未验证';
}

function formatBytes(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const order = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  const scaled = value / (1024 ** order);
  return `${scaled >= 100 || order === 0 ? scaled.toFixed(0) : scaled.toFixed(1)} ${units[order]}`;
}

function errorMessage(reason: unknown) {
  if (reason instanceof Error) return reason.message;
  if (reason && typeof reason === 'object' && 'message' in reason) {
    const message = (reason as { message?: unknown }).message;
    if (typeof message === 'string') return message;
  }
  return String(reason);
}

function operationFailure(reason: unknown): OperationFailureView {
  return {
    message: errorMessage(reason),
    operationId: operationIdFromError(reason),
  };
}

function operationIdFromError(reason: unknown) {
  if (!reason || typeof reason !== 'object' || !('operationId' in reason)) return undefined;
  const operationId = (reason as { operationId?: unknown }).operationId;
  return typeof operationId === 'string' && operationId.trim() ? operationId : undefined;
}

function correlatedFailure(reason: unknown, operationId?: string | null) {
  if (!operationId || operationIdFromError(reason)) return reason;
  return { message: errorMessage(reason), operationId };
}

export default App;
