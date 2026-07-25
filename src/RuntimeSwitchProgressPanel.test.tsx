import { render, screen, within } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import {
  RuntimeSwitchProgressPanel,
  type RuntimeSwitchFlow,
} from './RuntimeSwitchProgressPanel';
import type { RuntimeSwitchProgress } from './types';

function event(
  phase: RuntimeSwitchProgress['phase'],
  timestampMs: number,
  outcome?: RuntimeSwitchProgress['outcome'],
): RuntimeSwitchProgress {
  return { phase, timestampMs, outcome };
}

describe('RuntimeSwitchProgressPanel', () => {
  it('keeps the elapsed clock outside the phase-only live status', () => {
    const flow: RuntimeSwitchFlow = {
      status: 'running',
      target: 'relay',
      startedAtMs: 100,
      events: [event('verifyingRelay', 110)],
    };
    const view = render(<RuntimeSwitchProgressPanel flow={flow} now={1_100} />);
    const panel = screen.getByLabelText('运行态切换进度');
    const liveStatus = screen.getByRole('status');
    const clock = panel.querySelector('.switch-progress-clock');

    expect(panel.getAttribute('aria-live')).toBeNull();
    expect(liveStatus.textContent).toBe('验证中转站');
    expect(clock?.getAttribute('aria-hidden')).toBe('true');
    expect(clock?.textContent).toContain('1.0s');

    view.rerender(<RuntimeSwitchProgressPanel flow={flow} now={2_100} />);
    expect(screen.getByRole('status')).toBe(liveStatus);
    expect(liveStatus.textContent).toBe('验证中转站');
    expect(clock?.textContent).toContain('2.0s');

    const nextFlow = {
      ...flow,
      events: [...flow.events, event('detectingApp', 2_000)],
    };
    view.rerender(<RuntimeSwitchProgressPanel flow={nextFlow} now={2_100} />);
    expect(liveStatus.textContent).toBe('检测 ChatGPT');
  });

  it('shows rollback as running only while the task is still running', () => {
    const flow: RuntimeSwitchFlow = {
      status: 'running',
      target: 'relay',
      startedAtMs: 100,
      failedPhase: 'applyingRuntime',
      events: [
        event('verifyingRelay', 110),
        event('applyingRuntime', 120),
        event('rollingBack', 130),
      ],
    };

    render(<RuntimeSwitchProgressPanel flow={flow} now={140} />);

    expect(screen.getAllByText('正在恢复切换前状态')).toHaveLength(2);
    expect(screen.getByText('回滚完成后才会结束本次任务')).toBeTruthy();
    expect(screen.queryByText('已恢复切换前状态')).toBeNull();
  });

  it('renders a rolled-back terminal state and marks the interrupted phase', () => {
    const flow: RuntimeSwitchFlow = {
      status: 'failed',
      target: 'relay',
      startedAtMs: 100,
      completedAtMs: 150,
      failedPhase: 'applyingRuntime',
      error: 'runtime apply failed',
      events: [
        event('verifyingRelay', 110),
        event('applyingRuntime', 120),
        event('rollingBack', 130),
        event('failed', 150, 'rolledBack'),
      ],
    };

    render(<RuntimeSwitchProgressPanel flow={flow} now={200} />);

    expect(screen.queryByText('正在恢复切换前状态')).toBeNull();
    expect(screen.getByText('已恢复切换前状态')).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('切换失败，已恢复切换前状态');
    const interrupted = screen.getByText('应用运行态').closest('li');
    expect(interrupted?.className).toBe('failed');
    expect(within(interrupted as HTMLElement).getByText('中断')).toBeTruthy();
  });

  it('renders rollback failure as a distinct terminal state', () => {
    const flow: RuntimeSwitchFlow = {
      status: 'failed',
      target: 'plus',
      startedAtMs: 100,
      completedAtMs: 150,
      failedPhase: 'syncingToCurrent',
      error: 'rollback verification failed',
      events: [
        event('syncingToCurrent', 120),
        event('rollingBack', 130),
        event('failed', 150, 'rollbackFailed'),
      ],
    };

    render(<RuntimeSwitchProgressPanel flow={flow} now={200} />);

    expect(screen.queryByText('正在恢复切换前状态')).toBeNull();
    expect(screen.getByText('自动恢复未完成')).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('切换失败且自动恢复未完成');
    expect(screen.getByText('同步回本机').closest('li')?.className).toBe('failed');
  });

  it('marks a pre-write failure at its actual stage instead of completing it', () => {
    const flow: RuntimeSwitchFlow = {
      status: 'failed',
      target: 'relay',
      startedAtMs: 100,
      completedAtMs: 120,
      failedPhase: 'verifyingRelay',
      error: 'relay unreachable',
      events: [
        event('verifyingRelay', 110),
        event('failed', 120, 'failedBeforeWrite'),
      ],
    };

    render(<RuntimeSwitchProgressPanel flow={flow} now={200} />);

    const interrupted = screen.getByText('验证中转站').closest('li');
    expect(interrupted?.className).toBe('failed');
    expect(within(interrupted as HTMLElement).getByText('中断')).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('ChatGPT 数据未变更');
  });
});
