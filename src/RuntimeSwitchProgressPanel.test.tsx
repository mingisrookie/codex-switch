import { fireEvent, render, screen, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import {
  RuntimeSwitchProgressPanel,
  type RuntimeSwitchFlow,
} from './RuntimeSwitchProgressPanel';
import type {
  ChatGptLaunchStatus,
  RuntimeSwitchProgress,
  RuntimeSwitchResult,
} from './types';

function event(
  phase: RuntimeSwitchProgress['phase'],
  timestampMs: number,
  outcome?: RuntimeSwitchProgress['outcome'],
): RuntimeSwitchProgress {
  return { phase, timestampMs, outcome };
}

function result(
  status: ChatGptLaunchStatus,
  message: string | null = null,
): RuntimeSwitchResult {
  return {
    operationId: 'switch-1',
    changed: true,
    runtime: {
      id: 'relay',
      name: 'API 中转站',
      kind: 'relay',
      baseUrl: 'https://relay.example.com/v1',
      model: 'gpt-5.5',
      createdAtMs: 1,
      lastUsedAtMs: 2,
      lastVerifiedAtMs: 3,
    },
    incrementalSessionSync: {
      status: 'unchanged',
      detectedThreads: 0,
      syncedThreads: 0,
      projectedBytes: 0,
      durationMs: 20,
      requiresFullSync: false,
    },
    relayValidation: 'verified',
    chatProcessStateRepaired: false,
    chatgptLaunch: { status, message },
  };
}

function renderPanel(
  flow: RuntimeSwitchFlow,
  onClose = vi.fn(),
  onRetryLaunch = vi.fn(),
  closePending = false,
) {
  return {
    ...render(
      <RuntimeSwitchProgressPanel
        flow={flow}
        now={1_100}
        closePending={closePending}
        onClose={onClose}
        onRetryLaunch={onRetryLaunch}
      />,
    ),
    onClose,
    onRetryLaunch,
  };
}

describe('RuntimeSwitchProgressPanel', () => {
  it('renders one modal task overlay with a real current phase and elapsed time', () => {
    const flow: RuntimeSwitchFlow = {
      status: 'running',
      target: 'relay',
      startedAtMs: 100,
      events: [event('verifyingRelay', 110)],
    };

    renderPanel(flow);

    const dialog = screen.getByRole('dialog', { name: '正在切换到 API 中转站态' });
    expect(dialog.getAttribute('aria-modal')).toBe('true');
    expect(dialog.getAttribute('aria-busy')).toBe('true');
    expect(screen.getAllByRole('dialog')).toHaveLength(1);
    expect(screen.getByRole('region', { name: '切换步骤与回执' }).getAttribute('tabindex')).toBe('0');
    expect(screen.getByRole('status').textContent).toBe('验证中转站');
    expect(dialog.querySelector('.switch-task-clock')?.textContent).toContain('1.0s');
    expect(dialog.querySelector('.switch-timeline li.active')?.textContent).toContain('验证中转站');
    expect(dialog.querySelectorAll('.switch-timeline > li')).toHaveLength(7);
    expect(document.activeElement).toBe(within(dialog).getByRole('heading'));
  });

  it('cannot be dismissed with Escape while the backend command is unsettled', () => {
    const onClose = vi.fn();
    renderPanel({
      status: 'running',
      target: 'plus',
      startedAtMs: 100,
      events: [event('detectingApp', 110)],
    }, onClose);

    fireEvent.keyDown(screen.getByRole('dialog'), { key: 'Escape' });

    expect(onClose).not.toHaveBeenCalled();
    expect(screen.queryByRole('button', { name: '关闭任务' })).toBeNull();
  });

  it('shows the immutable official-auth check as a real backend phase', () => {
    renderPanel({
      status: 'running',
      target: 'plus',
      startedAtMs: 100,
      events: [event('validatingOfficialAuth', 150)],
    });

    const dialog = screen.getByRole('dialog');
    expect(screen.getByRole('status').textContent).toBe('准备目标与登录态');
    expect(dialog.querySelector('.switch-timeline li.active')?.textContent)
      .toContain('准备目标与登录态');
  });

  it('shows fine-grained phase timing, grouped steps, and a queued close receipt', () => {
    renderPanel({
      status: 'running',
      target: 'plus',
      startedAtMs: 100,
      events: [
        event('loadingRuntime', 100),
        event('validatingOfficialAuth', 200),
        event('detectingApp', 500),
        event('preparingRuntime', 700),
      ],
    }, vi.fn(), vi.fn(), true);

    expect(screen.getByText('本步骤已用 0.4s')).toBeTruthy();
    expect(screen.getByText('已收到关闭请求')).toBeTruthy();
    expect(screen.getByText(/到达可靠终态后会自动退出/)).toBeTruthy();
    expect(screen.getByText('准备')).toBeTruthy();
    expect(screen.getByText('保护')).toBeTruthy();
    expect(screen.getByText('切换')).toBeTruthy();
    expect(screen.getByText('收尾')).toBeTruthy();
    const authStep = screen.getByText('准备目标与登录态', { selector: 'strong' }).closest('li');
    expect(authStep?.textContent).toContain('0.4s');
  });

  it('identifies the slowest completed step in the terminal receipt', () => {
    renderPanel({
      status: 'succeeded',
      target: 'plus',
      startedAtMs: 100,
      completedAtMs: 1_100,
      events: [
        event('loadingRuntime', 100),
        event('validatingOfficialAuth', 100),
        event('detectingApp', 800),
        event('complete', 1_100),
      ],
      result: result('launched'),
    });

    expect(screen.getByText('耗时最长：准备目标与登录态 · 0.7s')).toBeTruthy();
  });

  it('does not present a complete progress event as terminal before the command result settles', () => {
    renderPanel({
      status: 'running',
      target: 'plus',
      startedAtMs: 100,
      events: [
        event('recordingResult', 150),
        event('launchingApp', 170),
        event('complete', 190),
      ],
    });

    const dialog = screen.getByRole('dialog');
    expect(dialog.getAttribute('aria-busy')).toBe('true');
    expect(screen.getByRole('status').textContent).toBe('正在确认任务终态');
    expect(screen.queryByText('ChatGPT 已打开')).toBeNull();
    expect(screen.queryByRole('button', { name: '完成' })).toBeNull();
  });

  it.each([
    ['launched' as const, 'ChatGPT 已打开'],
    ['alreadyRunning' as const, 'ChatGPT 已在运行'],
  ])('renders the %s launch receipt without a duplicate result surface', (status, label) => {
    renderPanel({
      status: 'succeeded',
      target: 'relay',
      startedAtMs: 100,
      completedAtMs: 220,
      events: [
        event('launchingApp', 180),
        event('complete', 210),
      ],
      result: result(status),
    });

    const dialog = screen.getByRole('dialog');
    expect(within(dialog).getAllByText(label).length).toBeGreaterThan(0);
    expect(within(dialog).getByText('switch-1')).toBeTruthy();
    expect(within(dialog).getByRole('button', { name: '完成' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: '重试打开 ChatGPT' })).toBeNull();
  });

  it('keeps switch success when launch fails and offers typed inline recovery actions', () => {
    const onClose = vi.fn();
    const onRetryLaunch = vi.fn();
    renderPanel({
      status: 'succeeded',
      target: 'relay',
      startedAtMs: 100,
      completedAtMs: 220,
      events: [
        event('recordingResult', 160),
        event('launchingApp', 180),
        event('complete', 210),
      ],
      result: result('failed', 'Windows activation was rejected'),
    }, onClose, onRetryLaunch);

    expect(screen.getAllByText('切换成功，ChatGPT 未能打开').length).toBeGreaterThan(0);
    expect(screen.getByRole('alert').textContent).toContain('Windows activation was rejected');
    const launchStep = screen.getByText('打开 ChatGPT', { selector: 'strong' }).closest('li');
    expect(launchStep?.className).toBe('failed');
    expect(launchStep?.textContent).toContain('中断');
    fireEvent.click(screen.getByRole('button', { name: '重试打开 ChatGPT' }));
    fireEvent.click(screen.getByRole('button', { name: '稍后手动打开' }));
    expect(onRetryLaunch).toHaveBeenCalledTimes(1);
    expect(onClose).toHaveBeenCalledTimes(1);
  });

  it('keeps ChatGPT closed without a retry action after incremental rollback failure', () => {
    const receipt = result(
      'blocked',
      'ChatGPT was kept closed because incremental session rollback failed.',
    );
    receipt.incrementalSessionSync = {
      status: 'failed',
      detectedThreads: 1,
      syncedThreads: 0,
      projectedBytes: 1024,
      durationMs: 450,
      requiresFullSync: true,
    };

    renderPanel({
      status: 'succeeded',
      target: 'plus',
      startedAtMs: 100,
      completedAtMs: 220,
      events: [
        event('syncingIncrementalSessions', 170),
        event('complete', 210),
      ],
      result: receipt,
    });

    expect(screen.getAllByText('切换成功，ChatGPT 已保持关闭').length).toBeGreaterThan(0);
    expect(screen.getByRole('alert').textContent).toContain('必须先检查操作记录与保留的安全检查点');
    expect(screen.queryByRole('button', { name: '重试打开 ChatGPT' })).toBeNull();
    expect(screen.queryByRole('button', { name: '稍后手动打开' })).toBeNull();
    expect(screen.getByRole('button', { name: '完成' })).toBeTruthy();
  });

  it('reports the auth-preserving request-route contract instead of session mutations', () => {
    const receipt = result('launched');
    receipt.chatProcessStateRepaired = true;
    receipt.incrementalSessionSync = {
      status: 'applied',
      detectedThreads: 2,
      syncedThreads: 2,
      projectedBytes: 2048,
      durationMs: 480,
      requiresFullSync: false,
    };
    renderPanel({
      status: 'succeeded',
      target: 'plus',
      startedAtMs: 100,
      completedAtMs: 200,
      events: [event('launchingApp', 180), event('complete', 190)],
      result: receipt,
    });

    expect(screen.getByText('目标请求端').nextSibling?.textContent).toBe('API Relay');
    expect(screen.getByText('官方登录态').nextSibling?.textContent).toBe('已验证保持不变');
    expect(screen.getByText('配置变更').nextSibling?.textContent).toBe('已原子应用');
    expect(screen.getByText('进程状态').nextSibling?.textContent).toBe('已安全修复');
    expect(screen.getByText('会话视图').nextSibling?.textContent)
      .toContain('已准备 2 条会话索引 · 0.5s');
  });

  it('does not claim process state was checked for an exact no-op', () => {
    const receipt = result('alreadyRunning');
    receipt.changed = false;
    renderPanel({
      status: 'succeeded',
      target: 'plus',
      startedAtMs: 100,
      completedAtMs: 200,
      events: [event('launchingApp', 180), event('complete', 190)],
      result: receipt,
    });

    expect(screen.getByText('进程状态').nextSibling?.textContent).toBe('未检查');
  });

  it('renders rollback as the switch terminal and never offers app launch', () => {
    renderPanel({
      status: 'failed',
      target: 'relay',
      startedAtMs: 100,
      completedAtMs: 150,
      operationId: 'switch-failed-1',
      failedPhase: 'applyingRuntime',
      error: 'runtime apply failed',
      events: [
        event('applyingRuntime', 120),
        event('rollingBack', 130),
        event('failed', 150, 'rolledBack'),
      ],
    });

    expect(screen.getByText('已恢复原始请求配置')).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('切换失败，已恢复原始请求配置');
    expect(screen.getByRole('button', { name: '导出本次诊断' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: /打开 ChatGPT/ })).toBeNull();
    const interrupted = screen.getByText('应用请求端配置', { selector: 'strong' }).closest('li');
    expect(interrupted?.className).toBe('failed');
  });

  it('allows terminal Escape dismissal and traps reverse Tab inside the dialog', () => {
    const onClose = vi.fn();
    renderPanel({
      status: 'succeeded',
      target: 'plus',
      startedAtMs: 100,
      completedAtMs: 200,
      events: [event('complete', 190)],
      result: result('launched'),
    }, onClose);
    const dialog = screen.getByRole('dialog');
    const heading = screen.getByRole('heading', { name: '切换到 ChatGPT 账号态' });
    const body = screen.getByRole('region', { name: '切换步骤与回执' });
    const complete = screen.getByRole('button', { name: '完成' });
    expect(document.activeElement).toBe(heading);

    fireEvent.keyDown(dialog, { key: 'Tab', shiftKey: true });
    expect(document.activeElement).toBe(complete);

    fireEvent.keyDown(dialog, { key: 'Tab' });
    expect(document.activeElement).toBe(body);

    fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
