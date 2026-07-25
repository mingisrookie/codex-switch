import {
  Check,
  CircleAlert,
  CloudDownload,
  CloudUpload,
  DatabaseBackup,
  HardDriveDownload,
  LoaderCircle,
  MonitorX,
  RadioTower,
  RotateCcw,
  Search,
  Settings2,
  ShieldCheck,
} from 'lucide-react';
import type { LucideIcon } from 'lucide-react';
import type { RuntimeKind, RuntimeSwitchPhase, RuntimeSwitchProgress } from './types';

export type RuntimeSwitchFlow = {
  status: 'running' | 'succeeded' | 'failed';
  target: RuntimeKind;
  events: RuntimeSwitchProgress[];
  startedAtMs: number;
  completedAtMs?: number;
  error?: string;
  failedPhase?: RuntimeSwitchPhase;
};

type Step = {
  phase: RuntimeSwitchPhase;
  label: string;
  icon: LucideIcon;
  relayOnly?: boolean;
  optional?: boolean;
};

const steps: Step[] = [
  { phase: 'verifyingRelay', label: '验证中转站', icon: RadioTower, relayOnly: true },
  { phase: 'detectingApp', label: '检测 ChatGPT', icon: Search },
  { phase: 'closingApp', label: '关闭 ChatGPT', icon: MonitorX, optional: true },
  { phase: 'backingUpCurrent', label: '备份当前数据', icon: HardDriveDownload },
  { phase: 'backingUpShared', label: '备份共享会话', icon: DatabaseBackup },
  { phase: 'syncingToShared', label: '同步至共享池', icon: CloudUpload },
  { phase: 'applyingRuntime', label: '应用运行态', icon: Settings2 },
  { phase: 'syncingToCurrent', label: '同步回本机', icon: CloudDownload },
  { phase: 'verifying', label: '校验切换结果', icon: ShieldCheck },
];

export function RuntimeSwitchProgressPanel({
  flow,
  now,
}: {
  flow: RuntimeSwitchFlow;
  now: number;
}) {
  const visibleSteps = steps.filter((step) => !step.relayOnly || flow.target === 'relay');
  const phaseIndexes = new Map(visibleSteps.map((step, index) => [step.phase, index]));
  const observed = new Set(flow.events.map((event) => event.phase));
  const latestWorkingEvent = [...flow.events]
    .reverse()
    .find((event) => phaseIndexes.has(event.phase) || event.phase === 'rollingBack');
  const currentIndex = latestWorkingEvent ? phaseIndexes.get(latestWorkingEvent.phase) ?? -1 : -1;
  const elapsedUntil = flow.completedAtMs ?? now;
  const elapsedSeconds = Math.max(0, (elapsedUntil - flow.startedAtMs) / 1000);
  const targetLabel = flow.target === 'plus' ? 'ChatGPT 账号' : 'API 中转站';
  const rollingBack = flow.status === 'running' && observed.has('rollingBack');
  const currentLabel = rollingBack
    ? '正在恢复切换前状态'
    : visibleSteps.find((step) => step.phase === latestWorkingEvent?.phase)?.label;
  const liveStatus = flow.status === 'running'
    ? currentLabel ?? '正在启动切换任务'
    : flow.status === 'succeeded'
      ? '切换完成，可以重新打开 ChatGPT'
      : '切换失败，请查看失败详情';

  return (
    <section
      className={`switch-progress ${flow.status}`}
      aria-label="运行态切换进度"
      aria-busy={flow.status === 'running'}
    >
      <span className="sr-only" role="status" aria-atomic="true">{liveStatus}</span>
      <header className="switch-progress-header">
        <div>
          <p className="eyebrow">任务执行器</p>
          <h2>{flow.status === 'running' ? `正在切换到${targetLabel}` : `切换到${targetLabel}`}</h2>
        </div>
        <div className="switch-progress-clock" aria-hidden="true">
          {flow.status === 'running' ? <LoaderCircle aria-hidden="true" /> : flow.status === 'succeeded'
            ? <Check aria-hidden="true" /> : <CircleAlert aria-hidden="true" />}
          <span>{elapsedSeconds.toFixed(1)}s</span>
        </div>
      </header>

      <ol className="switch-timeline">
        {visibleSteps.map((step, index) => {
          const state = stepState(flow, step, index, currentIndex, observed);
          const Icon = state === 'active' ? LoaderCircle : state === 'done' ? Check : step.icon;
          return (
            <li
              className={state}
              key={step.phase}
              aria-current={state === 'active' ? 'step' : undefined}
            >
              <span className="switch-step-icon"><Icon aria-hidden="true" /></span>
              <span className="switch-step-copy">
                <strong>{step.label}</strong>
                <small>{stepStatusLabel(state, step.optional)}</small>
              </span>
            </li>
          );
        })}
      </ol>

      {rollingBack ? (
        <div className="rollback-track">
          <RotateCcw aria-hidden="true" />
          <div><strong>正在恢复切换前状态</strong><span>回滚完成后才会结束本次任务</span></div>
        </div>
      ) : flow.status === 'failed' && flow.events.at(-1)?.outcome === 'rolledBack' ? (
        <div className="rollback-track complete">
          <Check aria-hidden="true" />
          <div><strong>已恢复切换前状态</strong><span>本次切换未生效，可以检查原因后重试</span></div>
        </div>
      ) : flow.status === 'failed' && flow.events.at(-1)?.outcome === 'rollbackFailed' ? (
        <div className="rollback-track failed">
          <CircleAlert aria-hidden="true" />
          <div><strong>自动恢复未完成</strong><span>请保持 ChatGPT 关闭，并使用已验证备份恢复</span></div>
        </div>
      ) : null}

      {flow.status === 'failed' ? (
        <p className="switch-terminal-error" role="alert">
          {failureLabel(flow.events.at(-1)?.outcome)}{flow.error ? `：${flow.error}` : ''}
        </p>
      ) : flow.status === 'succeeded' ? (
        <p className="switch-terminal-success">切换完成，可以重新打开 ChatGPT。会话索引将在打开会话页时刷新。</p>
      ) : <p className="switch-running-note">窗口会在任务结束前保持开启。</p>}
    </section>
  );
}

function stepState(
  flow: RuntimeSwitchFlow,
  step: Step,
  index: number,
  currentIndex: number,
  observed: Set<RuntimeSwitchPhase>,
) {
  if (flow.status === 'succeeded') return observed.has(step.phase) ? 'done' : 'skipped';
  if (flow.status === 'failed' && flow.failedPhase === step.phase) return 'failed';
  if (observed.has(step.phase) && index !== currentIndex) return 'done';
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

function failureLabel(outcome: RuntimeSwitchProgress['outcome']) {
  if (outcome === 'rolledBack') return '切换失败，已恢复切换前状态';
  if (outcome === 'rollbackFailed') return '切换失败且自动恢复未完成，请先不要重新打开 ChatGPT';
  return '切换在写入前失败，ChatGPT 数据未变更';
}
