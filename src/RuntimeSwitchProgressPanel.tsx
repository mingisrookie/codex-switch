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
import { DiagnosticExportAction } from './DiagnosticPanel';
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
  operationId?: string;
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
    description: '加载目标配置，并按目标锁定只读的 auth.json 状态',
    icon: ShieldCheck,
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
    id: 'incremental',
    phases: ['preparingRuntime', 'repairingAppState', 'syncingIncrementalSessions'],
    group: '保护',
    label: '准备增量会话视图',
    description: 'Relay 使用隔离索引；返回 Account 只发布新增会话',
    icon: RadioTower,
    optional: true,
  },
  {
    id: 'apply',
    phases: ['applyingRuntime'],
    group: '切换',
    label: '应用请求端配置',
    description: '原子切换请求路由和对应的会话索引',
    icon: Settings2,
  },
  {
    id: 'verify',
    phases: ['verifying', 'recordingResult'],
    group: '切换',
    label: '验证并记录请求端',
    description: '确认登录态、路由和会话索引匹配并持久化终态',
    icon: ShieldCheck,
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
              <div><strong>正在恢复原始请求配置</strong><span>只回滚 config.toml，auth.json 原字节或缺失状态不会被触碰</span></div>
            </div>
          ) : flow.status === 'failed' && latestEvent?.outcome === 'rolledBack' ? (
            <div className="rollback-track complete">
              <Check aria-hidden="true" />
              <div><strong>已恢复原始请求配置</strong><span>本次切换未生效，auth.json 原字节或缺失状态保持不变</span></div>
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
                <p>{failureGuidance(latestEvent?.reason)}</p>
                {flow.error ? <small>{flow.error}</small> : null}
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

          {flow.status === 'failed' && flow.operationId ? (
            <DiagnosticExportAction operationId={flow.operationId} />
          ) : null}

          {flow.refreshError ? (
            <p className="switch-refresh-warning" role="alert">
              切换已结束，但运行态刷新失败：{flow.refreshError}
            </p>
          ) : null}
        </div>

        {flow.status === 'failed' ? (
          <footer className="switch-task-footer">
            <p>写入前失败或已回滚时会尝试重新打开 ChatGPT；回滚失败则保持关闭。</p>
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
  const launchRetryable = launchFailed && (
    launch.reason === 'activationFailed' || launch.reason === 'verificationFailed' || !launch.reason
  );
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

      {launchProblem ? (
        <DiagnosticExportAction operationId={result.operationId} />
      ) : null}

      <dl className="switch-receipt" aria-label="切换回执">
        <div><dt>操作 ID</dt><dd>{result.operationId}</dd></div>
        <div><dt>目标请求端</dt><dd>{result.runtime.kind === 'plus' ? 'OpenAI 官方' : 'API Relay'}</dd></div>
        <div><dt>登录文件</dt><dd>{result.runtime.kind === 'relay' ? '未改写（允许不存在）' : '已验证保持不变'}</dd></div>
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
          <dt>会话视图</dt>
          <dd>{incrementalSyncLabel(result.incrementalSessionSync, result.runtime.kind)}</dd>
        </div>
        <div>
          <dt>回合来源</dt>
          <dd>{routeProvenanceLabel(result.routeProvenance.status)}</dd>
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
            ? '切换后安全记录未到达可验证终态；请先检查操作记录与保留的检查点，不要直接打开 ChatGPT。'
            : launchRetryable
            ? '运行态切换已经成功，不会因启动失败而回滚。'
            : launchFailed
              ? '运行态切换已经成功；启动目标需要先按上方说明恢复，当前不会猜测应用或 EXE。'
            : '切换回执已持久化；需要深度处理的会话保留给“会话合并与修复”。'}
        </p>
        <div className="switch-task-actions">
          {launchRetryable ? (
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

function incrementalSyncLabel(
  result: RuntimeSwitchResult['incrementalSessionSync'],
  runtimeKind: RuntimeKind,
) {
  const duration = `${(result.durationMs / 1000).toFixed(1)}s`;
  if (runtimeKind === 'relay' && result.status === 'applied') {
    return `已准备 ${result.detectedThreads} 条会话索引 · ${duration}`;
  }
  if (result.status === 'applied') return `已同步 ${result.syncedThreads} 个变化 · ${duration}`;
  if (result.status === 'unchanged') return `无变化 · ${duration}`;
  if (result.status === 'needsFullSync') return '需要会话合并与修复';
  if (result.status === 'deferred') return `已超出快速预算并延期 · ${duration}`;
  if (result.status === 'failed') return `未完成，需要会话合并与修复 · ${duration}`;
  return '本次无需执行';
}

function routeProvenanceLabel(status: RuntimeSwitchResult['routeProvenance']['status']) {
  if (status === 'recorded') return '已记录当前 provider、模型与账号槽位';
  if (status === 'unchanged') return '已验证现有来源基线';
  if (status === 'failed') return '未就绪，已阻止启动';
  return '等待记录';
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
    if (result.chatgptLaunch.reason === 'launchTargetAmbiguous') {
      return '切换结果仍然有效；检测到多个可信 Windows 应用身份，未猜测启动目标。请先手动打开希望使用的 ChatGPT/Codex 应用一次。';
    }
    if (result.chatgptLaunch.reason === 'launchTargetMissing') {
      return '切换结果仍然有效；尚未建立可验证的 Windows 应用身份。请先手动打开目标应用一次。';
    }
    if (result.chatgptLaunch.reason === 'processInventoryUnavailable') {
      return '切换结果仍然有效；本次无法验证目标应用进程，未继续自动启动。';
    }
    if (result.chatgptLaunch.reason === 'unsupported') {
      return '切换结果仍然有效；当前平台不支持受控自动启动。';
    }
    return '切换结果仍然有效，你可以重试自动打开或稍后手动打开。';
  }
  if (result.chatgptLaunch.status === 'blocked') {
    return '切换后安全记录未到达可验证终态；必须先检查操作记录与保留的安全检查点。';
  }
  return result.changed
    ? '后端没有请求启动应用，请查看任务说明。'
    : '配置与当前运行态一致，因此没有关闭或重新启动应用。';
}

function failureLabel(outcome: RuntimeSwitchProgress['outcome']) {
  if (outcome === 'rolledBack') return '切换失败，已恢复原始请求配置';
  if (outcome === 'rollbackFailed') return '切换失败且配置恢复未完成，请先不要重新打开 ChatGPT';
  if (outcome === 'failedBeforeWrite') return '切换在写入前失败，登录文件状态与请求配置均未变更';
  return '切换失败，但未收到可验证的终态；请先不要重新打开 ChatGPT';
}

function failureGuidance(reason: RuntimeSwitchProgress['reason']) {
  if (reason === 'officialAuthRequired') return 'Account 请求端需要现有的 ChatGPT 官方登录；Relay 配置没有被改动。';
  if (reason === 'invalidAuthState') return 'auth.json 存在但无法安全解析。修复或移走损坏文件后重试，应用不会代写登录态。';
  if (reason === 'configUnavailable') return '请求配置无法安全读取、写入或恢复。请检查 config.toml 权限与格式后重试。';
  if (reason === 'sessionViewUnavailable') return '会话数据库视图缺失、冲突或无法证明所有权。请保留现场并导出诊断。';
  if (reason === 'standaloneWriterActive') return '检测到独立 Codex CLI/工作进程仍在写入。请让对应任务自然结束或自行关闭后重试；应用不会终止它。';
  if (reason === 'mutationBusy') return '另一个真实写入任务正在执行。等待该任务到达终态后重试。';
  if (reason === 'processCloseFailed') return '受管 ChatGPT 进程未能安全退出。请先保存工作并手动关闭应用后重试。';
  if (reason === 'routeVerificationFailed') return '写入后的请求端校验未通过；系统已按终态尝试回滚，请查看诊断。';
  return '未收到可识别的失败原因。请导出诊断后重试，避免反复覆盖当前现场。';
}
