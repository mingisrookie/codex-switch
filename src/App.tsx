import { useEffect, useMemo, useRef, useState } from 'react';
import {
  ArrowLeftRight,
  Check,
  CircleAlert,
  CloudCog,
  Database,
  Download,
  History,
  KeyRound,
  LoaderCircle,
  MessagesSquare,
  RefreshCw,
  RotateCcw,
  Save,
  Settings2,
  ShieldCheck,
  UserRound,
  Wrench,
  X,
  Zap,
} from 'lucide-react';
import {
  checkForUpdates as defaultCheckForUpdates,
  closeCodexProcesses,
  deleteManagedSessions,
  dryRunAllSessions,
  importPlusRuntime,
  getAppStatus as defaultGetAppStatus,
  getUpdateStartupNotice as defaultGetUpdateStartupNotice,
  installUpdate as defaultInstallUpdate,
  listCodexProcesses,
  loadBackupDashboard as defaultLoadBackupDashboard,
  loadRuntimeDashboard as defaultLoadRuntimeDashboard,
  loadSessionDashboard as defaultLoadSessionDashboard,
  loadingDashboard,
  restoreBackup,
  restoreSessionsVisible,
  switchRuntime,
  syncAllSessions,
  upsertRelayRuntime,
  verifyRelayRuntime,
} from './api';
import { OperationResultPanel, type OperationView } from './OperationResultPanel';
import { RelayRuntimeDialog } from './RelayRuntimeDialog';
import {
  RuntimeSwitchProgressPanel,
  type RuntimeSwitchFlow,
} from './RuntimeSwitchProgressPanel';
import { SessionManagementPage } from './SessionManagementPage';
import { SkillsManagementPage } from './SkillsManagementPage';
import type {
  BackupSummary,
  AllSessionsDryRun,
  BackupDashboardData,
  DashboardData,
  DomainState,
  RelayRuntimeInput,
  RuntimeKind,
  RuntimeMetadata,
  RuntimeStatus,
  RuntimeDashboardData,
  RuntimeSwitchProgress,
  SessionDashboardData,
  OperationRecord,
  SessionMutationResult,
  SessionSyncResult,
  UpdateCheckResult,
} from './types';

type AppProps = {
  loadDashboard?: () => Promise<DashboardData>;
  loadRuntimeDashboard?: () => Promise<RuntimeDashboardData>;
  loadSessionDashboard?: () => Promise<SessionDashboardData>;
  loadBackupDashboard?: () => Promise<BackupDashboardData>;
};

type PendingConfirmation =
  | { kind: 'importAccount' }
  | { kind: 'syncSessions'; dryRun: AllSessionsDryRun }
  | { kind: 'restoreBackup'; backup: BackupSummary };

type RefreshScope = 'dashboard' | 'runtime' | 'none';
const numberFormat = new Intl.NumberFormat('zh-CN');

function App({
  loadDashboard,
  loadRuntimeDashboard = defaultLoadRuntimeDashboard,
  loadSessionDashboard = defaultLoadSessionDashboard,
  loadBackupDashboard = defaultLoadBackupDashboard,
}: AppProps) {
  const [data, setData] = useState<DashboardData>(() => loadingDashboard());
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [receipt, setReceipt] = useState<OperationView | null>(null);
  const [relayEditorOpen, setRelayEditorOpen] = useState(false);
  const [relaySubmitError, setRelaySubmitError] = useState<string | null>(null);
  const [pendingConfirmation, setPendingConfirmation] = useState<PendingConfirmation | null>(null);
  const [switchFlow, setSwitchFlow] = useState<RuntimeSwitchFlow | null>(null);
  const [clockNow, setClockNow] = useState(Date.now);
  const [sessionsStale, setSessionsStale] = useState(() => loadDashboard === undefined);
  const [backupsStale, setBackupsStale] = useState(() => loadDashboard === undefined);
  const [sessionRevision, setSessionRevision] = useState(0);
  const [activePage, setActivePage] = useState<'runtime' | 'sessions' | 'skills'>('runtime');
  const [appVersion, setAppVersion] = useState<string | null>(null);
  const [updateResult, setUpdateResult] = useState<UpdateCheckResult | null>(null);
  const [updateChecking, setUpdateChecking] = useState(false);
  const [updateInstalling, setUpdateInstalling] = useState(false);
  const [updateError, setUpdateError] = useState<string | null>(null);
  const [updateNotice, setUpdateNotice] = useState<string | null>(null);
  const [startupUpdateError, setStartupUpdateError] = useState<string | null>(null);
  const [dismissedUpdateVersion, setDismissedUpdateVersion] = useState<string | null>(null);
  const loadRequestId = useRef(0);
  const runtimeRequestId = useRef(0);
  const sessionRequestId = useRef(0);
  const backupRequestId = useRef(0);
  const requestedSessionRevision = useRef(-1);
  const switchAttemptId = useRef(0);
  const confirmationTrigger = useRef<HTMLElement | null>(null);
  const startupCheckStarted = useRef(false);
  const updateCheckInFlight = useRef(false);
  const exclusiveActionInFlight = useRef(false);

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
      .catch((reason: unknown) => { if (!cancelled && requestId === loadRequestId.current) setError(errorMessage(reason)); });
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
  }, []);

  useEffect(() => {
    if (switchFlow?.status !== 'running') return undefined;
    setClockNow(Date.now());
    const timer = window.setInterval(() => setClockNow(Date.now()), 250);
    return () => window.clearInterval(timer);
  }, [switchFlow?.status]);

  useEffect(() => {
    if (switchFlow?.status !== 'running') return undefined;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void import('@tauri-apps/api/window')
      .then(({ getCurrentWindow }) => getCurrentWindow().onCloseRequested((event) => {
        event.preventDefault();
      }))
      .then((stopListening) => {
        if (disposed) stopListening();
        else unlisten = stopListening;
      })
      .catch(() => undefined);
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [switchFlow?.status]);

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
    void refreshSessionDomains().catch((reason: unknown) => setError(errorMessage(reason)));
  }, [activePage, busy, sessionRevision, sessionsStale, switchFlow?.status, updateInstalling]);

  const codexHome = readyData(data.codexHome);
  const sessions = readyData(data.sessions);
  const managedSessions = readyData(data.managedSessions);
  const runtimes = readyData(data.runtimes);
  const runtimeStatus = readyData(data.runtimeStatus);
  const plusRuntime = useMemo(() => runtimes?.find((runtime) => runtime.kind === 'plus') ?? null, [runtimes]);
  const relayRuntime = useMemo(() => runtimes?.find((runtime) => runtime.kind === 'relay') ?? null, [runtimes]);

  const canImportAccount = Boolean(codexHome?.authJson.exists && codexHome.configToml.exists)
    && data.runtimes.status === 'ready';
  const canConfigureRelay = data.runtimes.status === 'ready'
    && (data.codexHome.status !== 'ready' || data.codexHome.data.configToml.exists);
  const canVerifyRelay = data.runtimes.status === 'ready' && Boolean(relayRuntime);
  const canSwitchRuntime = data.runtimes.status === 'ready';
  const canSync = !sessionsStale
    && data.sessions.status === 'ready'
    && data.managedSessions.status === 'ready';
  const canMutateSessions = data.managedSessions.status === 'ready';
  const canRestoreBackup = !backupsStale && data.backups.status === 'ready';
  const exclusiveBusy = busy !== null || updateInstalling;
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
    const next = await loadRuntimeDashboard();
    if (requestId === runtimeRequestId.current) {
      setData((current) => ({ ...current, ...next }));
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

  async function refreshBackupDomains() {
    const requestId = ++backupRequestId.current;
    const next = await loadBackupDashboard();
    if (requestId === backupRequestId.current) {
      setData((current) => ({ ...current, ...next }));
      setBackupsStale(false);
    }
  }

  function refreshInBackground(
    scope: RefreshScope = 'dashboard',
    onFailure?: (reason: unknown) => void,
  ) {
    if (scope === 'none') return;
    const task = scope === 'runtime' ? refreshRuntimeDomains() : refresh();
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
      void refreshSessionDomains().catch((reason: unknown) => setError(errorMessage(reason)));
      return;
    }
    refreshInBackground('runtime', (reason) => setError(errorMessage(reason)));
  }

  function handleLoadBackups() {
    if (exclusiveBusy || exclusiveActionInFlight.current) return;
    setData((current) => ({ ...current, backups: { status: 'loading' } }));
    void refreshBackupDomains().catch((reason: unknown) => setError(errorMessage(reason)));
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
      if (reportFailure) setUpdateError(errorMessage(reason));
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
      setUpdateError(errorMessage(reason));
      setUpdateInstalling(false);
      exclusiveActionInFlight.current = false;
    }
  }

  async function runAction<T>(
    label: string,
    action: () => Promise<T>,
    view: (result: T) => OperationView,
    onFailure?: (message: string) => void,
    refreshScope: RefreshScope = 'dashboard',
    onStart?: () => void,
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
        const message = errorMessage(reason);
        setError(message);
        onFailure?.(message);
        refreshInBackground(refreshScope);
        return null;
      }
      setReceipt(view(result));
      refreshInBackground(refreshScope, (reason) => {
        setError(`操作已成功，但状态刷新失败：${errorMessage(reason)}`);
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
    }), setRelaySubmitError, 'runtime');
    if (saved) setRelayEditorOpen(false);
  }

  async function handleVerifyRelay() {
    await runAction('验证中转站', verifyRelayRuntime, (runtime) => ({
      label: '中转站连接验证', metrics: [`验证时间：${formatTime(runtime.lastVerifiedAtMs)}`],
    }), undefined, 'runtime');
  }

  async function handleSwitch(runtimeId: RuntimeKind, label: string) {
    if (!canSwitchRuntime || busy !== null || updateInstalling || exclusiveActionInFlight.current) return;
    let attemptId: number | null = null;
    let mutationObserved = false;
    let backupObserved = false;

    const result = await runAction(label, () => switchRuntime(runtimeId, (event) => {
      if (attemptId !== switchAttemptId.current) return;
      if (!backupObserved && event.phase === 'backingUpShared') {
        backupObserved = true;
        markBackupsStale();
      }
      if (!mutationObserved && mutatesSessionData(event)) {
        mutationObserved = true;
        markSessionsStale();
      }
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
          status: event.phase === 'failed' ? 'failed'
            : event.phase === 'complete' ? 'succeeded'
            : current.status,
          events,
          failedPhase,
          completedAtMs: ['complete', 'failed'].includes(event.phase)
            ? event.timestampMs
            : current.completedAtMs,
        };
      });
    }), (switchResult) => ({
      label: switchResult.changed ? `${label}完成` : '运行态无需切换',
      operationId: switchResult.operationId,
      backupCount: switchResult.backups.length,
      backupPaths: switchResult.backups.map((backup) => backup.backupDir),
      rolledBack: switchResult.rolledBack,
      metrics: [
        `写入共享池：${switchResult.toShared.insertedThreads}`,
        `写回本机：${switchResult.fromShared.insertedThreads}`,
      ],
    }), (message) => {
      setSwitchFlow((current) => current ? {
        ...current,
        status: 'failed',
        error: message,
        failedPhase: current.failedPhase ?? lastRuntimeWorkPhase(current.events),
        completedAtMs: Date.now(),
      } : current);
    }, 'runtime', () => {
      attemptId = ++switchAttemptId.current;
      setClockNow(Date.now());
      setSwitchFlow({
        status: 'running',
        target: runtimeId,
        events: [],
        startedAtMs: Date.now(),
      });
    });

    if (attemptId === null || attemptId !== switchAttemptId.current) return;
    if (result) {
      if (result.changed && !mutationObserved) markSessionsStale();
      if (result.changed && !backupObserved) markBackupsStale();
      setSwitchFlow((current) => current ? {
        ...current,
        status: 'succeeded',
        completedAtMs: current.completedAtMs ?? Date.now(),
      } : current);
    }
  }

  async function handleSyncSessions() {
    if (!canSync || busy !== null || updateInstalling || exclusiveActionInFlight.current) return;
    exclusiveActionInFlight.current = true;
    setBusy('会话同步预检');
    setError(null);
    setReceipt(null);
    try {
      const dryRun = await dryRunAllSessions();
      requestConfirmation({ kind: 'syncSessions', dryRun });
    } catch (reason) {
      const message = errorMessage(reason);
      setError(message);
      refreshInBackground('dashboard');
    } finally {
      exclusiveActionInFlight.current = false;
      setBusy(null);
    }
  }

  async function performSyncSessions() {
    await runAction('同步会话', syncAllSessions, syncReceipt);
  }

  async function handleDeleteSessions(ids: string[], confirmed: boolean) {
    const result = await runAction('删除会话', async () => {
      await ensureChatGptClosed('会话删除');
      return deleteManagedSessions(ids, confirmed);
    }, mutationReceipt('会话删除完成'));
    return result !== null;
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

  async function confirmPendingAction() {
    const pending = pendingConfirmation;
    if (!pending) return;
    setPendingConfirmation(null);
    if (pending.kind === 'importAccount') {
      await importAccountRuntime(true);
    } else if (pending.kind === 'syncSessions') {
      await performSyncSessions();
    } else {
      await performRestoreBackup(pending.backup);
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
    <main className="app-shell">
      <header className="topbar">
        <div className="brand-lockup">
          <span className="brand-mark"><ArrowLeftRight aria-hidden="true" /></span>
          <span><strong>CHATGPT SWITCH</strong><small>LOCAL RUNTIME CONTROL</small></span>
        </div>
        <nav className="topbar-tabs" aria-label="主导航">
          <button aria-current={activePage === 'runtime' ? 'page' : undefined} className={`topbar-tab ${activePage === 'runtime' ? 'active' : ''}`} onClick={() => setActivePage('runtime')}><Zap aria-hidden="true" />运行态</button>
          <button aria-current={activePage === 'sessions' ? 'page' : undefined} className={`topbar-tab ${activePage === 'sessions' ? 'active' : ''}`} onClick={() => setActivePage('sessions')}><MessagesSquare aria-hidden="true" />会话</button>
          <button aria-current={activePage === 'skills' ? 'page' : undefined} className={`topbar-tab ${activePage === 'skills' ? 'active' : ''}`} onClick={() => setActivePage('skills')}><Wrench aria-hidden="true" />技能</button>
        </nav>
        <div className="topbar-actions">
          <span className="topbar-version" aria-live="polite">{versionStatus}</span>
          <button className="ghost-button" onClick={() => void runUpdateCheck(true)} disabled={updateChecking || exclusiveBusy}>
            {updateChecking ? <LoaderCircle className="button-icon spin" aria-hidden="true" /> : <Download className="button-icon" aria-hidden="true" />}
            {updateChecking ? '检查中' : updateInstalling ? '更新中' : '检查更新'}
          </button>
          {activePage !== 'skills' ? <button className="ghost-button" onClick={() => {
            handleManualRefresh();
          }} disabled={exclusiveBusy}><RefreshCw className="button-icon" aria-hidden="true" />刷新</button> : null}
        </div>
      </header>

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
      {updateError ? <p className="error-banner" role="alert"><strong>更新：</strong><span>{updateError}</span></p> : null}
      {activePage !== 'skills' ? domainErrors.map(({ domain, message }) => (
        <p className="error-banner" role="alert" key={domain}><strong>{domain}：</strong><span>{message}</span></p>
      )) : null}
      {activePage !== 'skills' && error ? <p className="error-banner" role="alert">{error}</p> : null}
      {busy ? <p className="busy-banner" role="status" aria-live="polite"><LoaderCircle className="spin" aria-hidden="true" />{busy}处理中</p> : null}
      {activePage !== 'skills' && receipt ? <OperationResultPanel result={receipt} /> : null}
      {pendingConfirmation ? (
        <InlineConfirmation
          pending={pendingConfirmation}
          busy={exclusiveBusy}
          onCancel={cancelPendingConfirmation}
          onConfirm={() => void confirmPendingAction()}
        />
      ) : null}
      {switchFlow ? <RuntimeSwitchProgressPanel flow={switchFlow} now={clockNow} /> : null}

      {activePage === 'runtime' ? (
        <section className="runtime-page">
          <header className="runtime-intro">
            <div className="runtime-intro-copy">
              <p className="eyebrow">本机运行态控制台</p>
              <h1><span>ChatGPT</span><ArrowLeftRight aria-hidden="true" /><span>API Relay</span></h1>
            </div>
            <dl className="runtime-readout" aria-label="当前扫描摘要">
              <div><dt>AUTH</dt><dd>{runtimeStatus?.authMode ?? authStatusLabel(data.codexHome)}</dd></div>
              <div><dt>RUNTIMES</dt><dd>{runtimes ? runtimes.length : statusLabel(data.runtimes)}</dd></div>
              <div><dt>THREADS</dt><dd>{threadCount}</dd></div>
              <div><dt>JSONL</dt><dd>{jsonlCount}</dd></div>
            </dl>
          </header>

          <section className="runtime-grid" aria-label="运行态">
            <RuntimeCard
              title="ChatGPT 账号态" kind="plus" description="本机账号登录态"
              runtime={plusRuntime} runtimeStatus={runtimeStatus} baseUrlFallback="本机 ChatGPT 登录态"
              runtimeDomainStatus={data.runtimes.status} runtimeStatusDomainStatus={data.runtimeStatus.status}
              onPrimary={() => void handleImportPlus()} primaryAction="保存当前账号态"
              onSwitch={() => void handleSwitch('plus', '切换 ChatGPT 账号')}
              switchAction={isExactRuntime(runtimeStatus, 'plus') ? '当前为 ChatGPT 账号' : runtimeStatus?.activeRuntimeId === 'plus' ? '重新应用 ChatGPT 账号' : '切换到 ChatGPT 账号'}
              primaryDisabled={exclusiveBusy || !canImportAccount}
              switchDisabled={exclusiveBusy || !canSwitchRuntime || !plusRuntime || isExactRuntime(runtimeStatus, 'plus')}
            />
            <RuntimeCard
              title="API 中转站态" kind="relay" description="URL、模型和加密保存的 API Key。"
              runtime={relayRuntime} runtimeStatus={runtimeStatus} baseUrlFallback="尚未配置"
              runtimeDomainStatus={data.runtimes.status} runtimeStatusDomainStatus={data.runtimeStatus.status}
              onPrimary={() => { setRelaySubmitError(null); setRelayEditorOpen(true); }} primaryAction="配置中转站"
              onSwitch={() => void handleSwitch('relay', '切换中转站')}
              switchAction={isExactRuntime(runtimeStatus, 'relay') ? '当前为中转站' : runtimeStatus?.activeRuntimeId === 'relay' ? '重新应用中转站' : '切换到中转站'}
              onVerify={() => void handleVerifyRelay()}
              primaryDisabled={exclusiveBusy || !canConfigureRelay}
              verifyDisabled={exclusiveBusy || !canVerifyRelay}
              switchDisabled={exclusiveBusy || !canSwitchRuntime || !relayRuntime || isExactRuntime(runtimeStatus, 'relay')}
            />
          </section>

          {relayEditorOpen ? (
            <RelayRuntimeDialog
              runtime={relayRuntime} fallbackModel={plusRuntime?.model ?? ''} busy={exclusiveBusy}
              submitError={relaySubmitError}
              onCancel={() => { setRelaySubmitError(null); setRelayEditorOpen(false); }} onSave={handleSaveRelay}
            />
          ) : null}

          <section className="runtime-operations" aria-label="数据与恢复">
            <aside className="detail-panel session-panel" aria-label="会话同步">
              <div className="card-title-row"><Database className="section-icon" aria-hidden="true" /><div><p className="eyebrow">SESSION POOL</p><h2>会话热同步</h2></div></div>
              <div className="sync-stats"><strong>{threadCount}<span>threads</span></strong><strong>{jsonlCount}<span>JSONL</span></strong></div>
              <button className="primary-button full" onClick={() => void handleSyncSessions()} disabled={exclusiveBusy || !canSync}><RefreshCw className="button-icon" aria-hidden="true" />立即同步</button>
            </aside>
            <SafetyPanel data={data} sessionsStale={sessionsStale} backupsStale={backupsStale} />
            <BackupRecoveryPanel
              state={data.backups}
              stale={backupsStale}
              disabled={exclusiveBusy || !canRestoreBackup}
              loadDisabled={exclusiveBusy}
              onLoad={handleLoadBackups}
              onRestore={handleRestoreBackup}
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
          onSync={() => void handleSyncSessions()} onDelete={handleDeleteSessions}
          onRestoreVisible={handleRestoreSessionsVisible}
        />
      ) : activePage === 'sessions' ? <DomainPlaceholder state={data.managedSessions} /> : null}

      <SkillsManagementPage
        active={activePage === 'skills'}
        busy={exclusiveBusy}
        onBusyChange={setBusy}
        ensureCodexClosed={ensureChatGptClosed}
      />
    </main>
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

  if (pending.kind === 'importAccount') {
    title = '覆盖已保存的 ChatGPT 账号态';
    detail = '当前账号态会先归档，再写入新的加密快照。';
    confirmLabel = '确认覆盖';
  } else if (pending.kind === 'syncSessions') {
    const newThreads = pending.dryRun.toShared.newThreads + pending.dryRun.toCurrent.newThreads;
    const duplicates = pending.dryRun.toShared.duplicateThreads
      + pending.dryRun.toCurrent.duplicateThreads;
    title = '会话同步预检已完成';
    detail = '确认后开始创建备份并执行双向同步。';
    metrics = [`新增 ${newThreads} 个线程`, `识别 ${duplicates} 个重复线程`];
    confirmLabel = '开始同步';
  } else {
    title = '恢复已验证备份';
    detail = `来源：${pending.backup.sourceRoot}`;
    metrics = [`快照：${pending.backup.backupDir}`];
    confirmLabel = '开始恢复';
  }

  return (
    <section className="inline-confirmation warning-confirmation" aria-label={title}>
      <CircleAlert className="section-icon" aria-hidden="true" />
      <div className="confirmation-copy">
        <p className="eyebrow">REVIEW ACTION</p>
        <h2 ref={headingRef} tabIndex={-1}>{title}</h2>
        <p>{detail}</p>
        {metrics.length > 0 ? <div className="confirmation-metrics">{metrics.map((metric) => <span key={metric}>{metric}</span>)}</div> : null}
      </div>
      <div className="confirmation-actions">
        <button className="ghost-button" onClick={onCancel} disabled={busy}><X className="button-icon" aria-hidden="true" />取消</button>
        <button className="warm-button" onClick={onConfirm} disabled={busy}><Check className="button-icon" aria-hidden="true" />{confirmLabel}</button>
      </div>
    </section>
  );
}

function RuntimeCard({
  title, kind, description, runtime, runtimeStatus, baseUrlFallback, primaryAction, switchAction,
  runtimeDomainStatus, runtimeStatusDomainStatus, onPrimary, onSwitch, onVerify,
  primaryDisabled, verifyDisabled = false, switchDisabled,
}: {
  title: string; kind: RuntimeKind; description: string; runtime: RuntimeMetadata | null;
  runtimeStatus: RuntimeStatus | null; baseUrlFallback: string; primaryAction: string; switchAction: string;
  runtimeDomainStatus: DomainState<RuntimeMetadata[]>['status'];
  runtimeStatusDomainStatus: DomainState<RuntimeStatus>['status'];
  onPrimary: () => void; onSwitch: () => void; onVerify?: () => void;
  primaryDisabled: boolean; verifyDisabled?: boolean; switchDisabled: boolean;
}) {
  const savedState = runtimeDomainStatus === 'ready'
    ? runtime ? '已保存' : '未保存'
    : domainStatusText(runtimeDomainStatus);
  const activeState = runtimeStatusDomainStatus !== 'ready'
    ? domainStatusText(runtimeStatusDomainStatus)
    : !runtimeStatus ? '未检测到' : runtimeStatus.activeRuntimeId === kind
    ? runtimeStatus.confidence === 'exact' ? '当前运行' : '模式匹配'
    : '非当前';
  const verifiedState = runtimeDomainStatus === 'ready'
    ? runtime?.lastVerifiedAtMs ? '已验证' : '未验证'
    : domainStatusText(runtimeDomainStatus);
  const runtimeDetailUnavailable = runtimeDomainStatus !== 'ready';
  const RuntimeIcon = kind === 'plus' ? UserRound : KeyRound;
  return (
    <article className={`runtime-card ${runtime?.kind ?? 'empty'}`} aria-label={title}>
      <div className="card-title-row"><RuntimeIcon className="section-icon" aria-hidden="true" /><div><p className="eyebrow">{kind === 'plus' ? 'ACCOUNT' : 'RELAY'}</p><h2>{title}</h2></div></div>
      <p className="runtime-description">{description}</p>
      <div className="runtime-state-grid">
        <span className={stateClass(savedState)}>{savedState}</span>
        <span className={stateClass(activeState)}>{activeState}</span>
        <span className={stateClass(verifiedState)}>{verifiedState}</span>
      </div>
      <dl className="meta-list">
        <div><dt>Base URL</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.baseUrl ?? baseUrlFallback}</dd></div>
        <div><dt>模型</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.model ?? '跟随当前 ChatGPT 配置'}</dd></div>
        <div><dt>最近验证</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.lastVerifiedAtMs ? formatTime(runtime.lastVerifiedAtMs) : '暂无验证记录'}</dd></div>
      </dl>
      <div className="runtime-actions">
        <button className="ghost-button inline" onClick={onPrimary} disabled={primaryDisabled}>{kind === 'plus' ? <Save className="button-icon" aria-hidden="true" /> : <Settings2 className="button-icon" aria-hidden="true" />}{primaryAction}</button>
        {onVerify ? <button className="ghost-button inline" onClick={onVerify} disabled={verifyDisabled || !runtime}><ShieldCheck className="button-icon" aria-hidden="true" />验证连接</button> : null}
        <button className="switch-button" onClick={onSwitch} disabled={switchDisabled}><ArrowLeftRight className="button-icon" aria-hidden="true" />{switchAction}</button>
      </div>
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
    </aside>
  );
}

function BackupRecoveryPanel({
  state,
  stale,
  disabled,
  loadDisabled,
  onLoad,
  onRestore,
}: {
  state: DomainState<BackupSummary[]>;
  stale: boolean;
  disabled: boolean;
  loadDisabled: boolean;
  onLoad: () => void;
  onRestore: (backup: BackupSummary) => void;
}) {
  const verifiedBackups = state.status === 'ready' ? state.data.filter((item) => item.verified).slice(0, 5) : [];
  return (
    <aside className="detail-panel backup-panel" aria-label="备份恢复">
      <div className="card-title-row"><RotateCcw className="section-icon" aria-hidden="true" /><div><p className="eyebrow">SNAPSHOTS</p><h2>备份恢复</h2></div></div>
      <p className="runtime-description">最近 5 份已验证快照</p>
      {stale ? (
        <div className="backup-load-state">
          <p>备份校验按需执行，避免与 ChatGPT 抢占磁盘。</p>
          <button className="ghost-button inline" onClick={onLoad} disabled={loadDisabled}>
            <RefreshCw className="button-icon" aria-hidden="true" />加载备份
          </button>
        </div>
      ) : state.status === 'error' ? <div className="backup-load-state" role="alert"><p>{state.error}</p><button className="ghost-button inline" onClick={onLoad} disabled={loadDisabled}><RefreshCw className="button-icon" aria-hidden="true" />重试</button></div>
        : state.status === 'loading' ? <p className="empty-state">备份列表扫描中...</p>
        : verifiedBackups.length > 0 ? <div className="backup-list">
        {verifiedBackups.map((backup) => <article className="backup-entry" key={backup.backupDir}>
          <dl className="compact-meta">
            <div><dt>原因</dt><dd>{backup.reason}</dd></div>
            <div><dt>时间</dt><dd>{formatTime(backup.createdAtMs)}</dd></div>
            <div><dt>文件</dt><dd>{backup.fileCount}</dd></div>
          </dl>
          <p className="backup-path" title={backup.sourceRoot}>来源：{backup.sourceRoot}</p>
          <p className="backup-path" title={backup.backupDir}>{backup.backupDir}</p>
          <button
            className="warm-button full"
            aria-label={`恢复此备份，${formatTime(backup.createdAtMs)}，来源 ${backup.sourceRoot}`}
            onClick={() => onRestore(backup)}
            disabled={disabled}
          ><RotateCcw className="button-icon" aria-hidden="true" />恢复此备份</button>
        </article>)}
      </div> : <p className="empty-state">没有可恢复的已验证备份。</p>}
    </aside>
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

function mutatesSessionData(event: RuntimeSwitchProgress) {
  return [
    'syncingToShared',
    'applyingRuntime',
    'syncingToCurrent',
    'rollingBack',
  ].includes(event.phase);
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
    switchRuntime: '切换运行态', syncSessions: '同步会话', deleteSessions: '删除会话',
    restoreVisibility: '恢复会话可见', restoreBackup: '恢复备份',
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
    ['运行态列表', data.runtimes], ['当前运行态', data.runtimeStatus], ['备份列表', data.backups],
    ['操作历史', data.operations],
  ];
  return domains.flatMap(([domain, state]) => state.status === 'error' ? [{ domain, message: state.error }] : []);
}

function syncReceipt(result: SessionSyncResult): OperationView {
  return {
    label: '会话同步完成', operationId: result.operationId, backupCount: result.backups?.length ?? 0,
    backupPaths: result.backups?.map((backup) => backup.backupDir),
    rolledBack: result.rolledBack,
    warnings: result.warnings,
    metrics: [`新增线程：${result.insertedThreads}`, `复制 JSONL：${result.copiedSessionFiles}`, `跳过缺失正文：${result.skippedMissingSessionFiles}`],
  };
}

function mutationReceipt(label: string) {
  return (result: SessionMutationResult): OperationView => ({
    label, operationId: result.operationId, backupCount: result.backups.length, rolledBack: result.rolledBack,
    backupPaths: result.backups.map((backup) => backup.backupDir),
    warnings: result.warnings,
    metrics: [`删除线程：${result.deletedThreads}`, `删除 JSONL：${result.deletedSessionFiles}`, `恢复线程：${result.restoredThreads}`],
  });
}

function formatTime(value: number | null) {
  return value ? new Date(value).toLocaleString('zh-CN', { hour12: false }) : '未验证';
}

function errorMessage(reason: unknown) {
  return reason instanceof Error ? reason.message : String(reason);
}

export default App;
