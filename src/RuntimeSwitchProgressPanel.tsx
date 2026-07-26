import { memo, useEffect, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import {
  AppWindow,
  Check,
  CircleAlert,
  CloudDownload,
  CloudUpload,
  DatabaseBackup,
  ExternalLink,
  HardDriveDownload,
  LoaderCircle,
  MonitorX,
  Power,
  RadioTower,
  RefreshCw,
  RotateCcw,
  Search,
  Settings2,
  ShieldCheck,
  Trash2,
  X,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import type {
  RuntimeKind,
  RuntimeSwitchPhase,
  RuntimeSwitchProgress,
  RuntimeSwitchResult,
} from './types';

export type RuntimeSwitchFlow = {
  status: 'running' | 'succeeded' | 'failed';
  target: RuntimeKind;
  events: RuntimeSwitchProgress[];
  startedAtMs: number;
  completedAtMs?: number;
  error?: string;
  failedPhase?: RuntimeSwitchPhase;
  result?: RuntimeSwitchResult;
  launchRetrying?: boolean;
  refreshError?: string;
};

type Step = {
  phase: RuntimeSwitchPhase;
  label: string;
  description: string;
  icon: LucideIcon;
  relayOnly?: boolean;
  optional?: boolean;
};

const steps: Step[] = [
  {
    phase: 'planningSessions',
    label: '规划写入范围',
    description: '计算会话写集与磁盘余量',
    icon: Search,
  },
  {
    phase: 'verifyingRelay',
    label: '验证中转站',
    description: '确认地址、模型与认证可用',
    icon: RadioTower,
    relayOnly: true,
  },
  {
    phase: 'detectingApp',
    label: '检测 ChatGPT',
    description: '识别受管应用与运行进程',
    icon: Search,
  },
  {
    phase: 'closingApp',
    label: '安全关闭 ChatGPT',
    description: '等待会话与数据库落盘',
    icon: MonitorX,
    optional: true,
  },
  {
    phase: 'backingUpCurrent',
    label: '建立本机检查点',
    description: '只覆盖本次实际写入状态',
    icon: HardDriveDownload,
  },
  {
    phase: 'backingUpShared',
    label: '建立共享检查点',
    description: '为共享池写入准备回滚点',
    icon: DatabaseBackup,
  },
  {
    phase: 'repairingAppState',
    label: '校验 Remote 连续性',
    description: '检查 ChatGPT 进程状态并安全修复',
    icon: RefreshCw,
  },
  {
    phase: 'syncingToShared',
    label: '同步至共享池',
    description: '合并本机会话的完整历史',
    icon: CloudUpload,
  },
  {
    phase: 'applyingRuntime',
    label: '应用运行态',
    description: '写入目标认证与模型配置',
    icon: Settings2,
  },
  {
    phase: 'syncingToCurrent',
    label: '同步回本机',
    description: '发布目标 provider 会话索引',
    icon: CloudDownload,
  },
  {
    phase: 'verifying',
    label: '校验切换结果',
    description: '核对配置、数据库与会话正文',
    icon: ShieldCheck,
  },
  {
    phase: 'cleaningCheckpoints',
    label: '释放临时检查点',
    description: '仅清理已有强终态证明的快照',
    icon: Trash2,
  },
  {
    phase: 'launchingApp',
    label: '打开 ChatGPT',
    description: '通过受控 Windows 应用入口启动',
    icon: AppWindow,
  },
];

export function RuntimeSwitchProgressPanel({
  flow,
  now: fixedNow,
  closeDisabled = false,
  onClose,
  onRetryLaunch,
}: {
  flow: RuntimeSwitchFlow;
  now?: number;
  closeDisabled?: boolean;
  onClose: () => void;
  onRetryLaunch: () => void;
}) {
  const headingRef = useRef<HTMLHeadingElement>(null);
  const dialogRef = useRef<HTMLDivElement>(null);
  const running = flow.status === 'running' || Boolean(flow.launchRetrying);
  const [clockNow, setClockNow] = useState(Date.now);
  const now = fixedNow ?? clockNow;

  useEffect(() => {
    headingRef.current?.focus();
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    return () => {
      document.body.style.overflow = previousOverflow;
    };
  }, []);

  useEffect(() => {
    if (!running || fixedNow !== undefined) return undefined;
    setClockNow(Date.now());
    const timer = window.setInterval(() => setClockNow(Date.now()), 250);
    return () => window.clearInterval(timer);
  }, [fixedNow, running]);

  function handleKeyDown(event: ReactKeyboardEvent<HTMLDivElement>) {
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      if (!running && !closeDisabled) onClose();
      return;
    }
    if (event.key !== 'Tab') return;

    const focusable = Array.from(dialogRef.current?.querySelectorAll<HTMLElement>(
      'button:not(:disabled), [href], input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])',
    ) ?? []).sort((left, right) => (
      left.compareDocumentPosition(right) & Node.DOCUMENT_POSITION_FOLLOWING ? -1 : 1
    ));
    if (focusable.length === 0) {
      event.preventDefault();
      headingRef.current?.focus();
      return;
    }
    const first = focusable[0];
    const last = focusable.at(-1);
    const active = document.activeElement;
    if (event.shiftKey && (active === first || active === headingRef.current)) {
      event.preventDefault();
      last?.focus();
    } else if (!event.shiftKey && active === headingRef.current) {
      event.preventDefault();
      first.focus();
    } else if (!event.shiftKey && active === last) {
      event.preventDefault();
      first.focus();
    }
  }

  const visibleSteps = steps.filter((step) => !step.relayOnly || flow.target === 'relay');
  const phaseIndexes = new Map(visibleSteps.map((step, index) => [step.phase, index]));
  const observed = new Set(flow.events.map((event) => event.phase));
  const latestEvent = flow.events.at(-1);
  const latestWorkingEvent = [...flow.events]
    .reverse()
    .find((event) => phaseIndexes.has(event.phase) || event.phase === 'rollingBack');
  const currentIndex = latestWorkingEvent ? phaseIndexes.get(latestWorkingEvent.phase) ?? -1 : -1;
  const elapsedUntil = flow.completedAtMs ?? now;
  const elapsedSeconds = Math.max(0, (elapsedUntil - flow.startedAtMs) / 1000);
  const targetLabel = flow.target === 'plus' ? 'ChatGPT 账号态' : 'API 中转站态';
  const rollingBack = flow.status === 'running' && observed.has('rollingBack');
  const waitingForReceipt = flow.status === 'running'
    && (latestEvent?.phase === 'complete' || latestEvent?.phase === 'failed');
  const currentStep = visibleSteps.find((step) => step.phase === latestWorkingEvent?.phase);
  const currentLabel = rollingBack
    ? '正在恢复切换前状态'
    : waitingForReceipt
      ? '正在确认任务终态'
      : currentStep?.label;
  const liveStatus = flow.launchRetrying
    ? '正在重新打开 ChatGPT'
    : flow.status === 'running'
      ? currentLabel ?? '正在启动切换任务'
      : flow.status === 'succeeded'
        ? launchTitle(flow.result)
        : '切换失败，请查看失败详情';

  const content = (
    <div className="switch-task-backdrop">
      <div
        ref={dialogRef}
        className={`switch-task-dialog ${flow.status}`}
        role="dialog"
        aria-modal="true"
        aria-labelledby="switch-task-title"
        aria-describedby="switch-task-summary"
        aria-busy={running}
        onKeyDown={handleKeyDown}
      >
        <SwitchLiveStatus message={liveStatus} />
        <header className="switch-task-header">
          <div className="switch-task-kicker">
            <span>任务执行器</span>
            <span>Runtime handoff</span>
          </div>
          <div className="switch-task-title-row">
            <div>
              <p className="eyebrow">{flow.status === 'running' ? 'SWITCH IN PROGRESS' : 'SWITCH RECEIPT'}</p>
              <h2 id="switch-task-title" ref={headingRef} tabIndex={-1}>
                {flow.status === 'running' ? `正在切换到 ${targetLabel}` : `切换到 ${targetLabel}`}
              </h2>
            </div>
            <div className="switch-task-clock" aria-hidden="true">
              {running ? <LoaderCircle className="spin" /> : flow.status === 'succeeded'
                ? <Check /> : <CircleAlert />}
              <span>{elapsedSeconds.toFixed(1)}s</span>
            </div>
          </div>
          <div className="switch-current-phase" id="switch-task-summary">
            <span className="switch-current-pulse" aria-hidden="true" />
            <div>
              <small>{flow.status === 'running' ? '当前阶段' : '任务终态'}</small>
              <strong>{liveStatus}</strong>
              {flow.status === 'running' && latestWorkingEvent?.message
                ? <p>{latestWorkingEvent.message}</p>
                : null}
            </div>
          </div>
        </header>

        <div
          className="switch-task-body"
          role="region"
          aria-label="切换步骤与回执"
          tabIndex={0}
        >
          <ol className="switch-timeline">
            {visibleSteps.map((step, index) => {
              const state = stepState(flow, step, index, currentIndex, observed);
              const Icon = state === 'active' ? LoaderCircle : state === 'done' ? Check : step.icon;
              const phaseEvent = flow.events.find((event) => event.phase === step.phase);
              return (
                <li
                  className={state}
                  key={step.phase}
                  aria-current={state === 'active' ? 'step' : undefined}
                >
                  <span className="switch-step-rail" aria-hidden="true" />
                  <span className="switch-step-icon">
                    <Icon className={state === 'active' ? 'spin' : undefined} aria-hidden="true" />
                  </span>
                  <span className="switch-step-copy">
                    <strong>{step.label}</strong>
                    <small>{step.description}</small>
                  </span>
                  <span className="switch-step-meta">
                    <strong>{stepStatusLabel(state, step.optional)}</strong>
                    <small>{phaseEvent ? phaseDuration(flow, phaseEvent, now) : '—'}</small>
                  </span>
                </li>
              );
            })}
          </ol>

          {rollingBack ? (
            <div className="rollback-track">
              <RotateCcw className="spin-slow" aria-hidden="true" />
              <div><strong>正在恢复切换前状态</strong><span>完成回滚和校验后才会结束任务</span></div>
            </div>
          ) : flow.status === 'failed' && latestEvent?.outcome === 'rolledBack' ? (
            <div className="rollback-track complete">
              <Check aria-hidden="true" />
              <div><strong>已恢复切换前状态</strong><span>本次切换未生效，可以检查原因后重试</span></div>
            </div>
          ) : flow.status === 'failed' && latestEvent?.outcome === 'rollbackFailed' ? (
            <div className="rollback-track failed">
              <CircleAlert aria-hidden="true" />
              <div><strong>自动恢复未完成</strong><span>请保持 ChatGPT 关闭，并使用已验证备份恢复</span></div>
            </div>
          ) : null}

          {flow.status === 'failed' ? (
            <section className="switch-terminal-card failed" role="alert">
              <CircleAlert aria-hidden="true" />
              <div>
                <strong>{failureLabel(latestEvent?.outcome)}</strong>
                {flow.error ? <p>{flow.error}</p> : null}
              </div>
            </section>
          ) : flow.status === 'succeeded' && flow.result ? (
            <SwitchSuccessResult
              flow={flow}
              closeDisabled={closeDisabled}
              onClose={onClose}
              onRetryLaunch={onRetryLaunch}
            />
          ) : (
            <p className="switch-running-note">
              正在执行受保护的切换链路。任务完成前无法关闭此窗口或操作后台页面。
            </p>
          )}

          {flow.refreshError ? (
            <p className="switch-refresh-warning" role="alert">
              切换已结束，但运行态刷新失败：{flow.refreshError}
            </p>
          ) : null}
        </div>

        {flow.status === 'failed' ? (
          <footer className="switch-task-footer">
            <p>失败或回滚状态不会自动打开 ChatGPT。</p>
            <button className="primary-button" onClick={onClose} disabled={closeDisabled}>
              <X className="button-icon" aria-hidden="true" />
              关闭任务
            </button>
          </footer>
        ) : null}
      </div>
    </div>
  );

  return createPortal(content, document.body);
}

function SwitchSuccessResult({
  flow,
  closeDisabled,
  onClose,
  onRetryLaunch,
}: {
  flow: RuntimeSwitchFlow;
  closeDisabled: boolean;
  onClose: () => void;
  onRetryLaunch: () => void;
}) {
  const result = flow.result;
  if (!result) return null;
  const launch = result.chatgptLaunch;
  const launchFailed = launch.status === 'failed';
  const LaunchIcon = flow.launchRetrying
    ? LoaderCircle
    : launchFailed
      ? CircleAlert
      : launch.status === 'notRequested'
        ? Power
        : AppWindow;
  const cleanup = result.checkpointCleanup;
  const retainedBackups = cleanup?.retainedCount ?? result.backups.length;
  const sessionBytesAdded = result.toShared.persistentSessionBytesAdded
    + result.fromShared.persistentSessionBytesAdded;
  const sessionBytesReclaimed = result.toShared.persistentSessionBytesReclaimed
    + result.fromShared.persistentSessionBytesReclaimed;
  const sessionBytesNet = sessionBytesAdded - sessionBytesReclaimed;

  return (
    <>
      <section
        className={`switch-terminal-card ${launchFailed ? 'warning' : 'success'}`}
        role={launchFailed ? 'alert' : undefined}
      >
        <LaunchIcon className={flow.launchRetrying ? 'spin' : undefined} aria-hidden="true" />
        <div>
          <strong>{flow.launchRetrying ? '正在重新打开 ChatGPT' : launchTitle(result)}</strong>
          <p>{launchDescription(result)}</p>
          {launch.message ? <small>{launch.message}</small> : null}
        </div>
      </section>

      <dl className="switch-receipt" aria-label="切换回执">
        <div><dt>操作 ID</dt><dd>{result.operationId}</dd></div>
        <div><dt>写入共享池</dt><dd>{result.toShared.insertedThreads}</dd></div>
        <div><dt>写回本机</dt><dd>{result.fromShared.insertedThreads}</dd></div>
        <div>
          <dt>Remote 状态</dt>
          <dd>
            {result.changed
              ? result.chatProcessStateRepaired ? '已修复' : '无需修复'
              : '未检查'}
          </dd>
        </div>
        <div><dt>保留检查点</dt><dd>{retainedBackups}</dd></div>
        <div><dt>会话新增占用</dt><dd>{formatBytes(sessionBytesAdded)}</dd></div>
        <div><dt>旧槽位回收</dt><dd>{formatBytes(sessionBytesReclaimed)}</dd></div>
        <div><dt>会话净变化</dt><dd>{formatSignedBytes(sessionBytesNet)}</dd></div>
        {cleanup ? (
          <div><dt>检查点回收</dt><dd>{formatBytes(cleanup.reclaimedBytes)}</dd></div>
        ) : null}
      </dl>

      {result.warnings?.length ? (
        <ul className="switch-warning-list" aria-label="切换说明">
          {result.warnings.map((warning) => <li key={warning}>{warning}</li>)}
        </ul>
      ) : null}

      <footer className="switch-task-footer">
        <p>
          {launchFailed
            ? '运行态切换已经成功，不会因启动失败而回滚。'
            : '切换回执已持久化，会话索引将在打开会话页时按需刷新。'}
        </p>
        <div className="switch-task-actions">
          {launchFailed ? (
            <button className="primary-button" onClick={onRetryLaunch} disabled={flow.launchRetrying}>
              {flow.launchRetrying
                ? <LoaderCircle className="button-icon spin" aria-hidden="true" />
                : <ExternalLink className="button-icon" aria-hidden="true" />}
              {flow.launchRetrying ? '正在打开' : '重试打开 ChatGPT'}
            </button>
          ) : null}
          <button
            className={launchFailed ? 'ghost-button' : 'primary-button'}
            onClick={onClose}
            disabled={flow.launchRetrying || closeDisabled}
          >
            {launchFailed ? <Power className="button-icon" aria-hidden="true" /> : <Check className="button-icon" aria-hidden="true" />}
            {launchFailed ? '稍后手动打开' : '完成'}
          </button>
        </div>
      </footer>
    </>
  );
}

const SwitchLiveStatus = memo(function SwitchLiveStatus({ message }: { message: string }) {
  return <span className="sr-only" role="status" aria-atomic="true">{message}</span>;
});

function stepState(
  flow: RuntimeSwitchFlow,
  step: Step,
  index: number,
  currentIndex: number,
  observed: Set<RuntimeSwitchPhase>,
) {
  if (flow.status === 'succeeded' && step.phase === 'launchingApp') {
    if (flow.launchRetrying) return 'active';
    if (flow.result?.chatgptLaunch.status === 'failed') return 'failed';
    if (flow.result?.chatgptLaunch.status === 'notRequested') return 'skipped';
    return observed.has(step.phase) ? 'done' : 'skipped';
  }
  if (flow.status === 'succeeded') return observed.has(step.phase) ? 'done' : 'skipped';
  if (flow.status === 'failed' && flow.failedPhase === step.phase) return 'failed';
  if (flow.status === 'failed' && observed.has(step.phase)) return 'done';
  if (observed.has(step.phase) && index !== currentIndex) return 'done';
  if (index === currentIndex && flow.status === 'running') return 'active';
  if (index === currentIndex && flow.status === 'failed') return 'failed';
  if (index < currentIndex) return 'skipped';
  return 'pending';
}

function formatSignedBytes(bytes: number) {
  if (bytes === 0) return '0 B';
  return `${bytes > 0 ? '+' : '−'}${formatBytes(Math.abs(bytes))}`;
}

function stepStatusLabel(state: string, optional = false) {
  if (state === 'done') return '已完成';
  if (state === 'active') return '执行中';
  if (state === 'failed') return '中断';
  if (state === 'skipped') return optional ? '无需执行' : '已跨过';
  return '等待';
}

function phaseDuration(
  flow: RuntimeSwitchFlow,
  event: RuntimeSwitchProgress,
  now: number,
) {
  const nextTimestamp = flow.events
    .filter((candidate) => candidate.timestampMs > event.timestampMs)
    .map((candidate) => candidate.timestampMs)
    .sort((left, right) => left - right)[0];
  const end = nextTimestamp ?? flow.completedAtMs ?? now;
  return `${Math.max(0, (end - event.timestampMs) / 1000).toFixed(1)}s`;
}

function launchTitle(result?: RuntimeSwitchResult) {
  if (!result) return '切换完成';
  if (result.chatgptLaunch.status === 'launched') return 'ChatGPT 已打开';
  if (result.chatgptLaunch.status === 'alreadyRunning') return 'ChatGPT 已在运行';
  if (result.chatgptLaunch.status === 'failed') return '切换成功，ChatGPT 未能打开';
  return result.changed ? '切换完成，未请求启动 ChatGPT' : '运行态无需切换';
}

function launchDescription(result: RuntimeSwitchResult) {
  if (result.chatgptLaunch.status === 'launched') {
    return '已通过受控的 Windows 应用入口完成启动。';
  }
  if (result.chatgptLaunch.status === 'alreadyRunning') {
    return '检测到目标应用已经运行，没有重复启动进程。';
  }
  if (result.chatgptLaunch.status === 'failed') {
    return '切换结果仍然有效，你可以重试或稍后手动打开。';
  }
  return result.changed
    ? '后端没有请求启动应用，请查看任务说明。'
    : '配置与当前运行态一致，因此没有关闭或重新启动应用。';
}

function failureLabel(outcome: RuntimeSwitchProgress['outcome']) {
  if (outcome === 'rolledBack') return '切换失败，已恢复切换前状态';
  if (outcome === 'rollbackFailed') return '切换失败且自动恢复未完成，请先不要重新打开 ChatGPT';
  if (outcome === 'failedBeforeWrite') return '切换在写入前失败，ChatGPT 数据未变更';
  return '切换失败，但未收到可验证的终态；请先不要重新打开 ChatGPT';
}

function formatBytes(value: number) {
  if (!Number.isFinite(value) || value <= 0) return '0 B';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB'];
  const order = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  const scaled = value / (1024 ** order);
  return `${scaled >= 100 || order === 0 ? scaled.toFixed(0) : scaled.toFixed(1)} ${units[order]}`;
}
