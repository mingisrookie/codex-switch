import { useEffect, useRef, useState } from 'react';
import {
  Download,
  ExternalLink,
  FolderOpen,
  LoaderCircle,
  RefreshCw,
  ShieldCheck,
  Trash2,
  X,
} from 'lucide-react';
import {
  clearDiagnosticLogs,
  exportDiagnostics,
  getDiagnosticStatus,
  normalizeDiagnosticExportFailure,
  openDiagnosticExport,
  openDiagnosticLogDirectory,
  retryDiagnosticExport,
} from './api';
import type { DiagnosticExportReceipt, DiagnosticStatus } from './types';

type DiagnosticState =
  | { status: 'loading' }
  | { status: 'ready'; data: DiagnosticStatus }
  | { status: 'error'; error: string };

type DiagnosticExportActionProps = {
  operationId?: string;
  buttonLabel?: string;
  showPrivacyNote?: boolean;
  disabled?: boolean;
  isBlocked?: () => boolean;
  onBusyChange?: (busy: boolean) => void;
};

type DiagnosticExportTarget = 'downloads' | 'diagnostic-directory';

export function DiagnosticExportAction({
  operationId,
  buttonLabel = operationId ? '导出本次诊断' : '导出最近诊断',
  showPrivacyNote = true,
  disabled = false,
  isBlocked,
  onBusyChange,
}: DiagnosticExportActionProps) {
  const [exportingTo, setExportingTo] = useState<DiagnosticExportTarget | null>(null);
  const [opening, setOpening] = useState(false);
  const [receipt, setReceipt] = useState<DiagnosticExportReceipt | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [retryId, setRetryId] = useState<string | null>(null);
  const taskInFlight = useRef(false);
  const primaryExportButtonRef = useRef<HTMLButtonElement>(null);
  const focusPrimaryAfterExport = useRef(false);

  useEffect(() => {
    if (!receipt || !focusPrimaryAfterExport.current) return;
    focusPrimaryAfterExport.current = false;
    primaryExportButtonRef.current?.focus();
  }, [receipt]);

  useEffect(() => {
    setReceipt(null);
    setRetryId(null);
    setError(null);
    focusPrimaryAfterExport.current = false;
  }, [operationId]);

  async function handleExport(target: DiagnosticExportTarget) {
    if (disabled || isBlocked?.() || taskInFlight.current) return;
    taskInFlight.current = true;
    setExportingTo(target);
    onBusyChange?.(true);
    setError(null);
    setReceipt(null);
    try {
      const nextReceipt = retryId
        ? await retryDiagnosticExport(
          retryId,
          target === 'downloads' ? 'downloads' : 'diagnosticDirectory',
        )
        : await exportDiagnostics(operationId);
      focusPrimaryAfterExport.current = target === 'diagnostic-directory';
      setReceipt(nextReceipt);
      setRetryId(null);
    } catch (reason) {
      const failure = normalizeDiagnosticExportFailure(reason);
      if (failure.kind === 'destination' && failure.retryId) {
        setRetryId(failure.retryId);
        setError(
          `${target === 'downloads' ? '下载目录导出失败' : '应用诊断目录导出失败'}：${failure.message}`,
        );
      } else {
        setRetryId(null);
        setError(`诊断导出失败：${failure.message}`);
      }
    } finally {
      taskInFlight.current = false;
      setExportingTo(null);
      onBusyChange?.(false);
    }
  }

  async function handleOpen() {
    if (!receipt || disabled || isBlocked?.() || taskInFlight.current) return;
    taskInFlight.current = true;
    setOpening(true);
    onBusyChange?.(true);
    setError(null);
    try {
      await openDiagnosticExport(receipt.exportId);
    } catch (reason) {
      setError(`打开导出位置失败：${errorMessage(reason)}`);
    } finally {
      taskInFlight.current = false;
      setOpening(false);
      onBusyChange?.(false);
    }
  }

  const exporting = exportingTo !== null;

  return (
    <div className="diagnostic-export-action">
      <div className="diagnostic-export-buttons">
        <button
          type="button"
          ref={primaryExportButtonRef}
          className="ghost-button"
          onClick={() => void handleExport('downloads')}
          disabled={disabled || exporting || opening}
        >
          {exportingTo === 'downloads'
            ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
            : <Download className="button-icon" aria-hidden="true" />}
          {exportingTo === 'downloads'
            ? '正在导出诊断'
            : retryId
              ? '重试下载目录'
              : buttonLabel}
        </button>
        {retryId && !receipt ? (
          <button
            type="button"
            className="ghost-button"
            onClick={() => void handleExport('diagnostic-directory')}
            disabled={disabled || exporting || opening}
          >
            {exportingTo === 'diagnostic-directory'
              ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
              : <FolderOpen className="button-icon" aria-hidden="true" />}
            {exportingTo === 'diagnostic-directory'
              ? '正在改存'
              : '改存应用诊断目录'}
          </button>
        ) : null}
      </div>
      {showPrivacyNote ? (
        <p className="diagnostic-privacy-note">
          <ShieldCheck aria-hidden="true" />已自动脱敏，不含凭据和聊天内容
        </p>
      ) : null}
      {receipt ? (
        <div className="diagnostic-export-receipt" role="status" aria-live="polite">
          <strong>诊断包已保存</strong>
          <span className="diagnostic-export-path" title={receipt.path}>{receipt.path}</span>
          <span>{formatBytes(receipt.bytes)} · {receipt.eventCount} 条事件</span>
          <button type="button" className="ghost-button inline" onClick={() => void handleOpen()} disabled={disabled || opening || exporting}>
            {opening
              ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
              : <ExternalLink className="button-icon" aria-hidden="true" />}
            {opening ? '正在打开' : '打开所在位置'}
          </button>
        </div>
      ) : null}
      {error ? <p className="diagnostic-action-error" role="alert">{error}</p> : null}
    </div>
  );
}

type DiagnosticPanelProps = {
  onClose: () => void;
  onBusyChange?: (busy: boolean) => void;
};

export function DiagnosticPanel({ onClose, onBusyChange }: DiagnosticPanelProps) {
  const [state, setState] = useState<DiagnosticState>({ status: 'loading' });
  const [openingLogs, setOpeningLogs] = useState(false);
  const [exportBusy, setExportBusy] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [clearConfirmation, setClearConfirmation] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const clearHeadingRef = useRef<HTMLHeadingElement>(null);
  const clearTriggerRef = useRef<HTMLButtonElement>(null);
  const panelTaskInFlight = useRef(false);

  useEffect(() => {
    const previousFocus = document.activeElement instanceof HTMLElement
      ? document.activeElement
      : null;
    headingRef.current?.scrollIntoView?.({ block: 'nearest' });
    headingRef.current?.focus();
    void refreshStatus();
    return () => {
      window.requestAnimationFrame(() => previousFocus?.focus());
    };
  }, []);

  useEffect(() => {
    if (!clearConfirmation) return;
    clearHeadingRef.current?.scrollIntoView?.({ block: 'nearest' });
    clearHeadingRef.current?.focus();
  }, [clearConfirmation]);

  async function refreshStatus() {
    setState({ status: 'loading' });
    try {
      setState({ status: 'ready', data: await getDiagnosticStatus() });
    } catch (reason) {
      setState({ status: 'error', error: errorMessage(reason) });
    }
  }

  async function handleOpenLogs() {
    if (panelTaskInFlight.current) return;
    panelTaskInFlight.current = true;
    setOpeningLogs(true);
    onBusyChange?.(true);
    setActionError(null);
    try {
      await openDiagnosticLogDirectory();
    } catch (reason) {
      setActionError(errorMessage(reason));
    } finally {
      panelTaskInFlight.current = false;
      setOpeningLogs(false);
      onBusyChange?.(false);
    }
  }

  function cancelClear() {
    setClearConfirmation(false);
    window.requestAnimationFrame(() => clearTriggerRef.current?.focus());
  }

  async function handleClear() {
    if (panelTaskInFlight.current) return;
    panelTaskInFlight.current = true;
    setClearing(true);
    onBusyChange?.(true);
    setActionError(null);
    try {
      await clearDiagnosticLogs();
      setClearConfirmation(false);
      await refreshStatus();
      window.requestAnimationFrame(() => clearTriggerRef.current?.focus());
    } catch (reason) {
      setActionError(errorMessage(reason));
    } finally {
      panelTaskInFlight.current = false;
      setClearing(false);
      onBusyChange?.(false);
    }
  }

  const closeDisabled = openingLogs || clearing || exportBusy;
  const status = state.status === 'ready' ? state.data : null;

  return (
    <section
      className="diagnostic-panel"
      id="diagnostic-panel"
      aria-labelledby="diagnostic-title"
      aria-describedby="diagnostic-privacy"
      onKeyDown={(event) => {
        if (event.key !== 'Escape' || closeDisabled) return;
        event.preventDefault();
        if (clearConfirmation) cancelClear();
        else onClose();
      }}
    >
      <header className="diagnostic-panel-heading">
        <div>
          <p className="eyebrow">SUPPORT DIAGNOSTICS</p>
          <h2 id="diagnostic-title" ref={headingRef} tabIndex={-1}>诊断与支持</h2>
        </div>
        <button
          className="icon-button"
          aria-label="关闭诊断面板"
          title="关闭诊断面板"
          onClick={onClose}
          disabled={closeDisabled}
        >
          <X aria-hidden="true" />
        </button>
      </header>

      <p className="diagnostic-privacy-note" id="diagnostic-privacy">
        <ShieldCheck aria-hidden="true" />已自动脱敏，不含凭据和聊天内容
      </p>

      {state.status === 'loading' ? (
        <p className="diagnostic-status-message" role="status">
          <LoaderCircle className="spin" aria-hidden="true" />正在读取诊断状态
        </p>
      ) : state.status === 'error' ? (
        <div className="diagnostic-status-message error">
          <p role="alert">诊断状态读取失败：{state.error}</p>
          <button className="ghost-button inline" onClick={() => void refreshStatus()}>
            <RefreshCw className="button-icon" aria-hidden="true" />重试
          </button>
        </div>
      ) : (
        <dl className="diagnostic-status-grid" aria-label="诊断日志状态">
          <div><dt>状态</dt><dd>{status?.available ? '可用' : '暂不可用'}</dd></div>
          <div><dt>事件</dt><dd>{status?.eventCount ?? 0}</dd></div>
          <div><dt>占用</dt><dd>{formatBytes(status?.totalBytes ?? 0)}</dd></div>
          <div><dt>保留</dt><dd>{status?.retentionDays ?? 14} 天 / {formatBytes(status?.maxBytes ?? 0)}</dd></div>
          <div><dt>最早记录</dt><dd>{formatTime(status?.oldestEventAtMs ?? null)}</dd></div>
          <div><dt>最近记录</dt><dd>{formatTime(status?.newestEventAtMs ?? null)}</dd></div>
        </dl>
      )}

      {(status?.warnings ?? []).map((warning) => (
        <p className="diagnostic-status-warning" key={warning}>{warning}</p>
      ))}

      <div className="diagnostic-panel-actions">
        <DiagnosticExportAction
          buttonLabel="导出最近诊断"
          showPrivacyNote={false}
          disabled={openingLogs || clearing}
          isBlocked={() => panelTaskInFlight.current}
          onBusyChange={(nextBusy) => {
            panelTaskInFlight.current = nextBusy;
            setExportBusy(nextBusy);
            onBusyChange?.(nextBusy);
          }}
        />
        <button className="ghost-button" onClick={() => void handleOpenLogs()} disabled={openingLogs || clearing || exportBusy}>
          {openingLogs
            ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
            : <FolderOpen className="button-icon" aria-hidden="true" />}
          {openingLogs ? '正在打开' : '打开日志目录'}
        </button>
        <button
          ref={clearTriggerRef}
          className="ghost-button danger"
          onClick={() => setClearConfirmation(true)}
          disabled={clearing || openingLogs || exportBusy || clearConfirmation}
        >
          <Trash2 className="button-icon" aria-hidden="true" />清除诊断日志
        </button>
      </div>

      {clearConfirmation ? (
        <section className="diagnostic-clear-confirmation" aria-labelledby="diagnostic-clear-title">
          <div>
            <p className="eyebrow">只清除诊断事件</p>
            <h3 id="diagnostic-clear-title" ref={clearHeadingRef} tabIndex={-1}>清除诊断日志？</h3>
            <p>不会删除操作历史、备份、会话、配置或凭据。</p>
          </div>
          <div className="form-actions">
            <button className="ghost-button inline" onClick={cancelClear} disabled={clearing}>取消</button>
            <button className="warm-button" onClick={() => void handleClear()} disabled={clearing}>
              {clearing
                ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
                : <Trash2 className="button-icon" aria-hidden="true" />}
              {clearing ? '正在清除' : '确认清除'}
            </button>
          </div>
        </section>
      ) : null}

      {actionError ? <p className="diagnostic-action-error" role="alert">{actionError}</p> : null}
    </section>
  );
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
}

function formatTime(value: number | null) {
  return value ? new Date(value).toLocaleString('zh-CN', { hour12: false }) : '暂无';
}

function errorMessage(reason: unknown) {
  if (reason instanceof Error) return reason.message;
  if (reason && typeof reason === 'object' && 'message' in reason) {
    const message = (reason as { message?: unknown }).message;
    if (typeof message === 'string') return message;
  }
  return String(reason);
}
