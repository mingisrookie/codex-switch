import { memo, useEffect, useRef, useState } from 'react';
import type { KeyboardEvent as ReactKeyboardEvent } from 'react';
import { createPortal } from 'react-dom';
import {
  AppWindow,
  Check,
  CircleAlert,
  ExternalLink,
  LoaderCircle,
  MonitorX,
  Power,
  RadioTower,
  RotateCcw,
  Settings2,
  ShieldCheck,
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
  id: string;
  phases: RuntimeSwitchPhase[];
  group: '准备' | '保护' | '切换' | '收尾';
  label: string;
  description: string;
  icon: LucideIcon;
  relayOnly?: boolean;
  optional?: boolean;
};

const steps: Step[] = [
  {
    id: 'prepare',
    phases: ['loadingRuntime', 'validatingOfficialAuth'],
    group: '准备',
    label: '准备目标与登录态',
    description: '加载目标配置，并锁定只读的官方 auth.json',
    icon: ShieldCheck,
  },
  {
    id: 'relay',
    phases: ['verifyingRelay'],
    group: '准备',
    label: '验证中转站',
    description: '确认地址、模型与认证可用',
    icon: RadioTower,
    relayOnly: true,
  },
  {
    id: 'close',
    phases: ['detectingApp', 'closingApp'],
    group: '准备',
    label: '安全关闭 ChatGPT',
    description: '识别受管进程并等待本地状态落盘',
    icon: MonitorX,
    optional: true,
  },
  {
    id: 'apply',
    phases: ['preparingRuntime', 'repairingAppState', 'applyingRuntime'],
    group: '保护',
    label: '应用最小配置补丁',
    description: '修复关闭状态并只原子替换 config.toml',
    icon: Settings2,
  },
  {
    id: 'verify',
    phases: ['verifying', 'recordingResult'],
    group: '切换',
    label: '验证并记录请求端',
    description: '确认登录态未变、路由匹配并持久化终态',
    icon: ShieldCheck,
  },
  {
    id: 'incremental',
    phases: ['syncingIncrementalSessions'],
    group: '收尾',
    label: '增量收口会话',
    description: '仅处理索引证明的小批量变化，超限立即延期',
    icon: RadioTower,
    optional: true,
  },
  {
    id: 'launch',
    phases: ['launchingApp'],
    group: '收尾',
    label: '打开 ChatGPT',
    description: '通过受控 Windows 应用入口启动',
    icon: AppWindow,
  },
];

export function RuntimeSwitchProgressPanel({
  flow,
  now: fixedNow,
  closeDisabled = false,
  closePending = false,
  onClose,
  onRetryLaunch,
}: {
  flow: RuntimeSwitchFlow;
  now?: number;
  closeDisabled?: boolean;
  closePending?: boolean;
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
  const phaseIndexes = new Map(
    visibleSteps.flatMap((step, index) => step.phases.map((phase) => [phase, index] as const)),
  );
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
  const currentStep = visibleSteps.find((step) => (
    latestWorkingEvent ? step.phases.includes(latestWorkingEvent.phase) : false
  ));
  const slowestStep = slowestObservedStep(flow, visibleSteps, now);
  const currentLabel = rollingBack
    ? '正在恢复原始请求配置'
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
              {flow.status === 'running' && latestWorkingEvent
                ? <p>本步骤已用 {phaseDuration(flow, latestWorkingEvent, now)}</p>
                : slowestStep
                  ? <p>耗时最长：{slowestStep.label} · {slowestStep.duration}</p>
                  : null}
            </div>
          </div>
          {closePending ? (
            <div className="switch-exit-pending" aria-live="polite">
              <Power aria-hidden="true" />
              <div>
                <strong>已收到关闭请求</strong>
                <span>正在安全完成当前步骤，到达可靠终态后会自动退出。</span>
              </div>
            </div>
          ) : null}
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
              const phaseEvent = stepStartEvent(flow, step);
              return (
                <li
                  className={state}
                  key={step.id}
                  aria-current={state === 'active' ? 'step' : undefined}
                >
                  {visibleSteps[index - 1]?.group !== step.group
                    ? <span className="switch-step-group">{step.group}</span>
                    : null}
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
                    <small>{phaseEvent ? stepDuration(flow, step, visibleSteps, now) : '—'}</small>
                  </span>
                </li>
              );
            })}
          </ol>

          {rollingBack ? (
            <div className="rollback-track">
              <RotateCcw className="spin-slow" aria-hidden="true" />
              <div><strong>正在恢复原始请求配置</strong><span>只回滚 config.toml，官方登录态不会被触碰</span></div>
            </div>
          ) : flow.status === 'failed' && latestEvent?.outcome === 'rolledBack' ? (
            <div className="rollback-track complete">
              <Check aria-hidden="true" />
              <div><strong>已恢复原始请求配置</strong><span>本次切换未生效，官方登录态保持不变</span></div>
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
  const launchBlocked = launch.status === 'blocked';
  const launchProblem = launchFailed || launchBlocked;
  const LaunchIcon = flow.launchRetrying
    ? LoaderCircle
    : launchProblem
      ? CircleAlert
      : launch.status === 'notRequested'
        ? Power
        : AppWindow;
  return (
    <>
      <section
        className={`switch-terminal-card ${launchProblem ? 'warning' : 'success'}`}
        role={launchProblem ? 'alert' : undefined}
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
        <div><dt>目标请求端</dt><dd>{result.runtime.kind === 'plus' ? 'OpenAI 官方' : 'API Relay'}</dd></div>
        <div><dt>官方登录态</dt><dd>已验证保持不变</dd></div>
        <div><dt>配置变更</dt><dd>{result.changed ? '已原子应用' : '无需变更'}</dd></div>
        <div>
          <dt>进程状态</dt>
          <dd>
            {result.changed
              ? result.chatProcessStateRepaired ? '已安全修复' : '无需修复'
              : '未检查'}
          </dd>
        </div>
        <div>
          <dt>会话增量</dt>
          <dd>{incrementalSyncLabel(result.incrementalSessionSync)}</dd>
        </div>
      </dl>

      {result.warnings?.length ? (
        <ul className="switch-warning-list" aria-label="切换说明">
          {result.warnings.map((warning) => <li key={warning}>{warning}</li>)}
        </ul>
      ) : null}

      <footer className="switch-task-footer">
        <p>
          {launchBlocked
            ? '增量会话未到达可验证的安全终态；请先检查操作记录与保留的检查点，不要直接打开 ChatGPT。'
            : launchFailed
            ? '运行态切换已经成功，不会因启动失败而回滚。'
            : '切换回执已持久化；超出快速预算的会话维护保留为手动完全同步。'}
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
            className={launchProblem ? 'ghost-button' : 'primary-button'}
            onClick={onClose}
            disabled={flow.launchRetrying || closeDisabled}
          >
            {launchProblem ? <Power className="button-icon" aria-hidden="true" /> : <Check className="button-icon" aria-hidden="true" />}
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
  const observedStep = step.phases.some((phase) => observed.has(phase));
  if (flow.status === 'succeeded' && step.phases.includes('launchingApp')) {
    if (flow.launchRetrying) return 'active';
    if (
      flow.result?.chatgptLaunch.status === 'failed'
      || flow.result?.chatgptLaunch.status === 'blocked'
    ) return 'failed';
    if (flow.result?.chatgptLaunch.status === 'notRequested') return 'skipped';
    return observedStep ? 'done' : 'skipped';
  }
  if (flow.status === 'succeeded') return observedStep ? 'done' : 'skipped';
  if (flow.status === 'failed' && step.phases.includes(flow.failedPhase ?? 'failed')) return 'failed';
  if (flow.status === 'failed' && observedStep) return 'done';
  if (observedStep && index !== currentIndex) return 'done';
  if (index === currentIndex && flow.status === 'running') return 'active';
  if (index === currentIndex && flow.status === 'failed') return 'failed';
  if (index < currentIndex) return 'skipped';
  return 'pending';
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
  return `${(phaseDurationMs(flow, event, now) / 1000).toFixed(1)}s`;
}

function stepStartEvent(flow: RuntimeSwitchFlow, step: Step) {
  return flow.events.find((event) => step.phases.includes(event.phase));
}

function stepDuration(
  flow: RuntimeSwitchFlow,
  step: Step,
  visibleSteps: Step[],
  now: number,
) {
  const durationMs = stepDurationMs(flow, step, visibleSteps, now);
  return `${(durationMs / 1000).toFixed(1)}s`;
}

function stepDurationMs(
  flow: RuntimeSwitchFlow,
  step: Step,
  visibleSteps: Step[],
  now: number,
) {
  const event = stepStartEvent(flow, step);
  if (!event) return 0;
  const eventIndex = flow.events.indexOf(event);
  const laterStep = flow.events.slice(eventIndex + 1).find((candidate) => (
    visibleSteps.some((other) => other.id !== step.id && other.phases.includes(candidate.phase))
  ));
  const end = laterStep?.timestampMs ?? flow.completedAtMs ?? now;
  return Math.max(0, end - event.timestampMs);
}

function phaseDurationMs(
  flow: RuntimeSwitchFlow,
  event: RuntimeSwitchProgress,
  now: number,
) {
  const eventIndex = flow.events.indexOf(event);
  const nextTimestamp = eventIndex >= 0
    ? flow.events[eventIndex + 1]?.timestampMs
    : undefined;
  const end = nextTimestamp ?? flow.completedAtMs ?? now;
  return Math.max(0, end - event.timestampMs);
}

function slowestObservedStep(
  flow: RuntimeSwitchFlow,
  visibleSteps: Step[],
  now: number,
) {
  if (flow.status === 'running') return null;
  return visibleSteps
    .map((step) => {
      const event = stepStartEvent(flow, step);
      if (!event) return null;
      const durationMs = stepDurationMs(flow, step, visibleSteps, now);
      return { label: step.label, durationMs, duration: `${(durationMs / 1000).toFixed(1)}s` };
    })
    .filter((item): item is NonNullable<typeof item> => item !== null)
    .sort((left, right) => right.durationMs - left.durationMs)[0] ?? null;
}

function incrementalSyncLabel(result: RuntimeSwitchResult['incrementalSessionSync']) {
  const duration = `${(result.durationMs / 1000).toFixed(1)}s`;
  if (result.status === 'applied') return `已同步 ${result.syncedThreads} 个变化 · ${duration}`;
  if (result.status === 'unchanged') return `无变化 · ${duration}`;
  if (result.status === 'needsFullSync') return '需要手动完全同步';
  if (result.status === 'deferred') return `已超出快速预算并延期 · ${duration}`;
  if (result.status === 'failed') return `未完成，需要手动完全同步 · ${duration}`;
  return '本次无需执行';
}

function launchTitle(result?: RuntimeSwitchResult) {
  if (!result) return '切换完成';
  if (result.chatgptLaunch.status === 'launched') return 'ChatGPT 已打开';
  if (result.chatgptLaunch.status === 'alreadyRunning') return 'ChatGPT 已在运行';
  if (result.chatgptLaunch.status === 'failed') return '切换成功，ChatGPT 未能打开';
  if (result.chatgptLaunch.status === 'blocked') return '切换成功，ChatGPT 已保持关闭';
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
  if (result.chatgptLaunch.status === 'blocked') {
    return '增量会话未到达可验证终态；必须先检查操作记录与保留的安全检查点。';
  }
  return result.changed
    ? '后端没有请求启动应用，请查看任务说明。'
    : '配置与当前运行态一致，因此没有关闭或重新启动应用。';
}

function failureLabel(outcome: RuntimeSwitchProgress['outcome']) {
  if (outcome === 'rolledBack') return '切换失败，已恢复原始请求配置';
  if (outcome === 'rollbackFailed') return '切换失败且配置恢复未完成，请先不要重新打开 ChatGPT';
  if (outcome === 'failedBeforeWrite') return '切换在写入前失败，官方登录态与请求配置均未变更';
  return '切换失败，但未收到可验证的终态；请先不要重新打开 ChatGPT';
}
