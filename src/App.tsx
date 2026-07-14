import { useEffect, useMemo, useRef, useState } from 'react';
import {
  closeCodexProcesses,
  deleteManagedSessions,
  dryRunAllSessions,
  importPlusRuntime,
  listCodexProcesses,
  loadDashboard as defaultLoadDashboard,
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
import { SessionManagementPage } from './SessionManagementPage';
import { SkillsManagementPage } from './SkillsManagementPage';
import type {
  BackupSummary,
  DashboardData,
  DomainState,
  RelayRuntimeInput,
  RuntimeKind,
  RuntimeMetadata,
  RuntimeStatus,
  OperationRecord,
  SessionMutationResult,
  SessionSyncResult,
} from './types';

type AppProps = { loadDashboard?: () => Promise<DashboardData> };
const numberFormat = new Intl.NumberFormat('zh-CN');

function App({ loadDashboard = defaultLoadDashboard }: AppProps) {
  const [data, setData] = useState<DashboardData>(() => loadingDashboard());
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [receipt, setReceipt] = useState<OperationView | null>(null);
  const [relayDialogOpen, setRelayDialogOpen] = useState(false);
  const [relaySubmitError, setRelaySubmitError] = useState<string | null>(null);
  const [activePage, setActivePage] = useState<'runtime' | 'sessions' | 'skills'>('runtime');
  const loadRequestId = useRef(0);

  useEffect(() => {
    let cancelled = false;
    const requestId = ++loadRequestId.current;
    loadDashboard()
      .then((next) => { if (!cancelled && requestId === loadRequestId.current) setData(next); })
      .catch((reason: unknown) => { if (!cancelled && requestId === loadRequestId.current) setError(errorMessage(reason)); });
    return () => { cancelled = true; };
  }, [loadDashboard]);

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
  const canSync = data.sessions.status === 'ready' && data.managedSessions.status === 'ready';
  const canMutateSessions = data.managedSessions.status === 'ready';
  const canRestoreBackup = data.backups.status === 'ready';
  const threadCount = sessions ? numberFormat.format(sessions.threadCount) : statusLabel(data.sessions);
  const jsonlCount = sessions ? numberFormat.format(sessions.sessionJsonlCount) : statusLabel(data.sessions);

  async function refresh() {
    const requestId = ++loadRequestId.current;
    const next = await loadDashboard();
    if (requestId === loadRequestId.current) setData(next);
  }

  function refreshInBackground(onFailure?: (reason: unknown) => void) {
    const requestId = loadRequestId.current + 1;
    void refresh().catch((reason: unknown) => {
      if (requestId === loadRequestId.current) onFailure?.(reason);
    });
  }

  async function runAction<T>(
    label: string,
    action: () => Promise<T>,
    view: (result: T) => OperationView,
    onFailure?: (message: string) => void,
  ) {
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
        refreshInBackground();
        return null;
      }
      setReceipt(view(result));
      refreshInBackground((reason) => {
        setError(`操作已成功，但状态刷新失败：${errorMessage(reason)}`);
      });
      return result;
    } finally {
      setBusy(null);
    }
  }

  async function ensureCodexClosed(reason: string) {
    const processes = await listCodexProcesses();
    if (processes.length === 0) return;
    if (!window.confirm(`检测到 ${processes.length} 个 Codex 进程。${reason}需要关闭后继续，是否关闭？`)) {
      throw new Error('用户取消操作');
    }
    await closeCodexProcesses();
  }

  async function handleImportPlus() {
    if (!canImportAccount) return;
    const overwrite = Boolean(plusRuntime);
    if (overwrite && !window.confirm('已保存 Codex 账号态。覆盖前会保留历史快照，确认继续？')) return;
    await runAction('保存 Codex 账号态', () => importPlusRuntime(overwrite), (runtime) => ({
      label: 'Codex 账号态已保存', metrics: [`运行态：${runtime.name}`],
    }));
  }

  async function handleSaveRelay(input: RelayRuntimeInput) {
    setRelaySubmitError(null);
    const saved = await runAction('配置中转站', () => upsertRelayRuntime(input), (runtime) => ({
      label: 'API 中转站已保存', metrics: [`模型：${runtime.model ?? '未设置'}`],
    }), setRelaySubmitError);
    if (saved) setRelayDialogOpen(false);
  }

  async function handleVerifyRelay() {
    await runAction('验证中转站', verifyRelayRuntime, (runtime) => ({
      label: '中转站连接验证', metrics: [`验证时间：${formatTime(runtime.lastVerifiedAtMs)}`],
    }));
  }

  async function handleSwitch(runtimeId: RuntimeKind, label: string) {
    await runAction(label, async () => {
      await ensureCodexClosed('运行态切换');
      return switchRuntime(runtimeId);
    }, (result) => ({
      label: result.changed ? `${label}完成` : '运行态无需切换',
      operationId: result.operationId,
      backupCount: result.backups.length,
      backupPaths: result.backups.map((backup) => backup.backupDir),
      rolledBack: result.rolledBack,
      metrics: [
        `写入共享池：${result.toShared.insertedThreads}`,
        `写回当前 Home：${result.fromShared.insertedThreads}`,
      ],
    }));
  }

  async function handleSyncSessions() {
    if (!canSync) return;
    setBusy('会话同步预检');
    setError(null);
    setReceipt(null);
    try {
      const dryRun = await dryRunAllSessions();
      const newThreads = dryRun.toShared.newThreads + dryRun.toCurrent.newThreads;
      const duplicates = dryRun.toShared.duplicateThreads + dryRun.toCurrent.duplicateThreads;
      if (!window.confirm(`同步预检完成：预计新增 ${newThreads} 个线程，识别 ${duplicates} 个重复线程。确认执行热同步？`)) return;
      const result = await syncAllSessions();
      setReceipt(syncReceipt(result));
      refreshInBackground((reason) => {
        setError(`操作已成功，但状态刷新失败：${errorMessage(reason)}`);
      });
    } catch (reason) {
      const message = errorMessage(reason);
      setError(message);
      refreshInBackground();
    } finally {
      setBusy(null);
    }
  }

  async function handleDeleteSessions(ids: string[], confirmed: boolean) {
    const result = await runAction('删除会话', async () => {
      await ensureCodexClosed('会话删除');
      return deleteManagedSessions(ids, confirmed);
    }, mutationReceipt('会话删除完成'));
    return result !== null;
  }

  async function handleRestoreSessionsVisible(ids: string[]) {
    const result = await runAction('恢复会话可见', async () => {
      await ensureCodexClosed('恢复会话可见');
      return restoreSessionsVisible(ids);
    }, mutationReceipt('会话恢复完成'));
    return result !== null;
  }

  async function handleRestoreBackup(backup: BackupSummary) {
    if (!backup.verified || !canRestoreBackup) return;
    if (!window.confirm(`将使用已验证备份恢复其来源 Codex Home：\n${backup.sourceRoot}\n备份：${backup.backupDir}\n确认继续？`)) return;
    await runAction('恢复备份', async () => {
      await ensureCodexClosed('备份恢复');
      return restoreBackup(backup.backupDir);
    }, (result) => ({
      label: '备份恢复完成', operationId: result.operationId,
      rolledBack: result.rolledBack, warnings: result.warnings,
      backupCount: result.safetyBackup ? 1 : 0,
      backupPaths: result.safetyBackup ? [result.safetyBackup.backupDir] : [],
      metrics: [`恢复文件：${result.restoredFiles}`],
    }));
  }

  const domainErrors = dashboardErrors(data);

  return (
    <main className="app-shell">
      <header className="topbar">
        <div className="brand-dot">CS</div><strong>CODEX SWITCH</strong>
        <nav className="topbar-tabs" aria-label="主导航">
          <button aria-current={activePage === 'runtime' ? 'page' : undefined} className={`topbar-tab ${activePage === 'runtime' ? 'active' : ''}`} onClick={() => setActivePage('runtime')}>运行态</button>
          <button aria-current={activePage === 'sessions' ? 'page' : undefined} className={`topbar-tab ${activePage === 'sessions' ? 'active' : ''}`} onClick={() => setActivePage('sessions')}>会话管理</button>
          <button aria-current={activePage === 'skills' ? 'page' : undefined} className={`topbar-tab ${activePage === 'skills' ? 'active' : ''}`} onClick={() => setActivePage('skills')}>技能</button>
        </nav>
        {activePage !== 'skills' ? <button className="ghost-button" onClick={() => {
          setError(null);
          refreshInBackground((reason) => setError(errorMessage(reason)));
        }} disabled={busy !== null}>刷新</button> : null}
      </header>

      {activePage !== 'skills' ? domainErrors.map(({ domain, message }) => (
        <p className="error-banner" role="alert" key={domain}><strong>{domain}：</strong><span>{message}</span></p>
      )) : null}
      {activePage !== 'skills' && error ? <p className="error-banner" role="alert">{error}</p> : null}
      {busy ? <p className="busy-banner" role="status" aria-live="polite">{busy}处理中...</p> : null}
      {activePage !== 'skills' && receipt ? <OperationResultPanel result={receipt} /> : null}

      {activePage === 'runtime' ? (
        <>
          <section className="hero-card account-hero">
            <div>
              <p className="eyebrow">Codex 账号态 ↔ API 中转站</p><h1>Codex 运行态切换</h1>
              <p className="lede">保存态、当前态和验证态分别展示。写操作只在依赖扫描成功后启用，切换与恢复均保留后端回执。</p>
              <div className="hero-meta" aria-label="当前扫描摘要">
                <span>auth：{authStatusLabel(data.codexHome)}</span>
                <span>运行态：{runtimes ? runtimes.length : statusLabel(data.runtimes)}</span>
                <span>会话：{threadCount}</span><span>JSONL：{jsonlCount}</span>
              </div>
            </div>
          </section>

          <section className="runtime-grid" aria-label="运行态与会话">
            <RuntimeCard
              title="Codex 账号态" kind="plus" description="本机 Codex 账号登录态，凭据加密保存。"
              runtime={plusRuntime} runtimeStatus={runtimeStatus} baseUrlFallback="本机 Codex 登录态"
              runtimeDomainStatus={data.runtimes.status} runtimeStatusDomainStatus={data.runtimeStatus.status}
              onPrimary={() => void handleImportPlus()} primaryAction="保存当前账号态"
              onSwitch={() => void handleSwitch('plus', '切换 Codex 账号')}
              switchAction={isExactRuntime(runtimeStatus, 'plus') ? '当前为 Codex 账号' : runtimeStatus?.activeRuntimeId === 'plus' ? '重新应用 Codex 账号' : '切换到 Codex 账号'}
              primaryDisabled={busy !== null || !canImportAccount}
              switchDisabled={busy !== null || !canSwitchRuntime || !plusRuntime || isExactRuntime(runtimeStatus, 'plus')}
            />
            <RuntimeCard
              title="API 中转站态" kind="relay" description="URL、模型和加密保存的 API Key。"
              runtime={relayRuntime} runtimeStatus={runtimeStatus} baseUrlFallback="尚未配置"
              runtimeDomainStatus={data.runtimes.status} runtimeStatusDomainStatus={data.runtimeStatus.status}
              onPrimary={() => { setRelaySubmitError(null); setRelayDialogOpen(true); }} primaryAction="配置中转站"
              onSwitch={() => void handleSwitch('relay', '切换中转站')}
              switchAction={isExactRuntime(runtimeStatus, 'relay') ? '当前为中转站' : runtimeStatus?.activeRuntimeId === 'relay' ? '重新应用中转站' : '切换到中转站'}
              onVerify={() => void handleVerifyRelay()}
              primaryDisabled={busy !== null || !canConfigureRelay}
              verifyDisabled={busy !== null || !canVerifyRelay}
              switchDisabled={busy !== null || !canSwitchRuntime || !relayRuntime || isExactRuntime(runtimeStatus, 'relay')}
            />

            <aside className="detail-panel session-panel" aria-label="会话同步">
              <div className="card-title-row"><span className="card-icon">🔄</span><div><p className="eyebrow">先预检再写入</p><h2>会话热同步</h2></div></div>
              <p className="hint no-indent">执行前展示双向 dry-run 统计；完成后展示真实新增、复制、跳过和备份回执。</p>
              <div className="sync-stats"><strong>{threadCount}<span>threads</span></strong><strong>{jsonlCount}<span>JSONL</span></strong></div>
              <button className="primary-button full" onClick={() => void handleSyncSessions()} disabled={busy !== null || !canSync}>立即同步</button>
            </aside>

            <SafetyPanel data={data} />
            <BackupRecoveryPanel state={data.backups} disabled={busy !== null || !canRestoreBackup} onRestore={handleRestoreBackup} />
            <OperationHistoryPanel state={data.operations} />
          </section>
        </>
      ) : activePage === 'sessions' && managedSessions ? (
        <SessionManagementPage
          inventory={managedSessions} busy={busy !== null}
          syncDisabled={!canSync} mutationDisabled={!canMutateSessions}
          onSync={() => void handleSyncSessions()} onDelete={handleDeleteSessions}
          onRestoreVisible={handleRestoreSessionsVisible}
        />
      ) : activePage === 'sessions' ? <DomainPlaceholder state={data.managedSessions} /> : null}

      <SkillsManagementPage
        active={activePage === 'skills'}
        busy={busy !== null}
        onBusyChange={setBusy}
        ensureCodexClosed={ensureCodexClosed}
      />

      {relayDialogOpen ? (
        <RelayRuntimeDialog
          runtime={relayRuntime} fallbackModel={plusRuntime?.model ?? ''} busy={busy !== null}
          submitError={relaySubmitError}
          onCancel={() => { setRelaySubmitError(null); setRelayDialogOpen(false); }} onSave={handleSaveRelay}
        />
      ) : null}
    </main>
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
  return (
    <article className={`runtime-card ${runtime?.kind ?? 'empty'}`} aria-label={title}>
      <div className="card-title-row"><span className="card-icon">{kind === 'plus' ? '👤' : '🔑'}</span><div><p className="eyebrow">{kind === 'plus' ? '账号登录态' : 'API 调用态'}</p><h2>{title}</h2></div></div>
      <p className="hint no-indent">{description}</p>
      <div className="runtime-state-grid">
        <span className={stateClass(savedState)}>{savedState}</span>
        <span className={stateClass(activeState)}>{activeState}</span>
        <span className={stateClass(verifiedState)}>{verifiedState}</span>
      </div>
      <dl className="meta-list">
        <div><dt>Base URL</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.baseUrl ?? baseUrlFallback}</dd></div>
        <div><dt>模型</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.model ?? '跟随当前 Codex 配置'}</dd></div>
        <div><dt>最近验证</dt><dd>{runtimeDetailUnavailable ? domainStatusText(runtimeDomainStatus) : runtime?.lastVerifiedAtMs ? formatTime(runtime.lastVerifiedAtMs) : '暂无验证记录'}</dd></div>
      </dl>
      <div className="runtime-actions">
        <button className="ghost-button inline" onClick={onPrimary} disabled={primaryDisabled}>{primaryAction}</button>
        {onVerify ? <button className="ghost-button inline" onClick={onVerify} disabled={verifyDisabled || !runtime}>验证连接</button> : null}
        <button className="switch-button" onClick={onSwitch} disabled={switchDisabled}>{switchAction}</button>
      </div>
    </article>
  );
}

function SafetyPanel({ data }: { data: DashboardData }) {
  const home = readyData(data.codexHome);
  const status = readyData(data.runtimeStatus);
  const backups = readyData(data.backups);
  const latestBackup = backups?.[0];
  const homeFilesReady = Boolean(home?.authJson.exists && home.configToml.exists && home.stateDb.exists);
  const homeState = data.codexHome.status === 'ready' ? homeFilesReady ? '完整' : '缺失' : statusLabel(data.codexHome);
  const backupState = data.backups.status === 'ready'
    ? latestBackup?.verified ? '已验证' : '无已验证备份'
    : statusLabel(data.backups);
  return (
    <aside className="detail-panel safety-panel" aria-label="安全检查">
      <div className="card-title-row"><span className="card-icon">🛡️</span><div><p className="eyebrow">实时状态</p><h2>切换保护</h2></div></div>
      <SafetyLine ok={homeFilesReady} label={`Codex Home 核心文件：${homeState}`} />
      <SafetyLine ok={status?.confidence === 'exact'} label={`运行态检测：${status?.confidence ?? statusLabel(data.runtimeStatus)}`} />
      <SafetyLine ok={Boolean(latestBackup?.verified)} label={`最近备份：${backupState}`} />
      <SafetyLine ok={data.sessions.status === 'ready'} label={`会话扫描：${statusLabel(data.sessions)}`} />
    </aside>
  );
}

function BackupRecoveryPanel({ state, disabled, onRestore }: { state: DomainState<BackupSummary[]>; disabled: boolean; onRestore: (backup: BackupSummary) => void }) {
  const verifiedBackups = state.status === 'ready' ? state.data.filter((item) => item.verified).slice(0, 5) : [];
  return (
    <aside className="detail-panel backup-panel" aria-label="备份恢复">
      <div className="card-title-row"><span className="card-icon">↩</span><div><p className="eyebrow">完整快照</p><h2>备份恢复</h2></div></div>
      <p className="hint no-indent">仅校验并展示最近 5 份备份候选；旧备份不会自动清理。</p>
      {state.status === 'error' ? <p className="empty-state" role="alert">{state.error}</p>
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
          >恢复此备份</button>
        </article>)}
      </div> : <p className="empty-state">没有可恢复的已验证备份。</p>}
    </aside>
  );
}

function OperationHistoryPanel({ state }: { state: DomainState<OperationRecord[]> }) {
  return (
    <aside className="detail-panel operation-history-panel" aria-label="操作历史">
      <div className="card-title-row"><span className="card-icon">📋</span><div><p className="eyebrow">本机持久化记录</p><h2>操作历史</h2></div></div>
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

function SafetyLine({ ok, label }: { ok: boolean; label: string }) {
  return <div className={`safety-line ${ok ? '' : 'warning'}`}><span>{ok ? '✓' : '!'}</span><strong>{label}</strong></div>;
}

function DomainPlaceholder<T>({ state }: { state: DomainState<T> }) {
  return <section className="hero-card domain-placeholder">{state.status === 'error' ? state.error : '会话数据扫描中...'}</section>;
}

function readyData<T>(state: DomainState<T>): T | null {
  return state.status === 'ready' ? state.data : null;
}

function isExactRuntime(status: RuntimeStatus | null, runtimeId: RuntimeKind) {
  return status?.activeRuntimeId === runtimeId && status.confidence === 'exact';
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
    ['Codex Home', data.codexHome], ['会话扫描', data.sessions], ['会话管理', data.managedSessions],
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
