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
  const sync = {
    insertedThreads: 0,
    copiedSessionFiles: 0,
    duplicateThreads: 0,
    skippedMissingSessionFiles: 0,
    skippedArchivedThreads: 0,
    mergedSessionIndexEntries: 0,
    persistentSessionBytesAdded: 0,
    persistentSessionBytesReclaimed: 0,
  };
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
    backups: [],
    toShared: { ...sync },
    fromShared: { ...sync },
    rolledBack: false,
    chatProcessStateRepaired: false,
    chatgptLaunch: { status, message },
  };
}

function renderPanel(
  flow: RuntimeSwitchFlow,
  onClose = vi.fn(),
  onRetryLaunch = vi.fn(),
) {
  return {
    ...render(
      <RuntimeSwitchProgressPanel
        flow={flow}
        now={1_100}
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

  it('shows the Remote continuity check as a real backend phase', () => {
    renderPanel({
      status: 'running',
      target: 'plus',
      startedAtMs: 100,
      events: [event('repairingAppState', 150)],
    });

    const dialog = screen.getByRole('dialog');
    expect(screen.getByRole('status').textContent).toBe('校验 Remote 连续性');
    expect(dialog.querySelector('.switch-timeline li.active')?.textContent)
      .toContain('校验 Remote 连续性');
  });

  it('does not present a complete progress event as terminal before the command result settles', () => {
    renderPanel({
      status: 'running',
      target: 'plus',
      startedAtMs: 100,
      events: [
        event('cleaningCheckpoints', 150),
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
        event('cleaningCheckpoints', 160),
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

  it('uses typed retained counts and reports persistent session storage separately', () => {
    const receipt = result('launched');
    receipt.backups = [
      { backupDir: 'C:\\backup-a', sourceRoot: 'C:\\home', reason: 'a', createdAtMs: 1, scope: 'runtimeState', trackedDatabaseCount: 1, completeSessions: false },
      { backupDir: 'C:\\backup-b', sourceRoot: 'C:\\shared', reason: 'b', createdAtMs: 1, scope: 'stateOnly', trackedDatabaseCount: 1, completeSessions: false },
    ];
    receipt.checkpointCleanup = {
      attemptedCount: 2,
      reclaimedCount: 1,
      reclaimedBytes: 4096,
      retainedCount: 1,
      failedCount: 0,
      warnings: [],
    };
    receipt.toShared.persistentSessionBytesAdded = 8192;
    receipt.fromShared.persistentSessionBytesReclaimed = 2048;
    receipt.chatProcessStateRepaired = true;
    renderPanel({
      status: 'succeeded',
      target: 'plus',
      startedAtMs: 100,
      completedAtMs: 200,
      events: [event('launchingApp', 180), event('complete', 190)],
      result: receipt,
    });

    expect(screen.getByText('保留检查点').nextSibling?.textContent).toBe('1');
    expect(screen.getByText('会话新增占用').nextSibling?.textContent).toBe('8.0 KiB');
    expect(screen.getByText('旧槽位回收').nextSibling?.textContent).toBe('2.0 KiB');
    expect(screen.getByText('会话净变化').nextSibling?.textContent).toBe('+6.0 KiB');
    expect(screen.getByText('Remote 状态').nextSibling?.textContent).toBe('已修复');
    expect(screen.getByText('检查点回收').nextSibling?.textContent).toBe('4.0 KiB');
  });

  it('does not claim Remote state was checked for an exact no-op', () => {
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

    expect(screen.getByText('Remote 状态').nextSibling?.textContent).toBe('未检查');
  });

  it('renders rollback as the switch terminal and never offers app launch', () => {
    renderPanel({
      status: 'failed',
      target: 'relay',
      startedAtMs: 100,
      completedAtMs: 150,
      failedPhase: 'applyingRuntime',
      error: 'runtime apply failed',
      events: [
        event('applyingRuntime', 120),
        event('rollingBack', 130),
        event('failed', 150, 'rolledBack'),
      ],
    });

    expect(screen.getByText('已恢复切换前状态')).toBeTruthy();
    expect(screen.getByRole('alert').textContent).toContain('切换失败，已恢复切换前状态');
    expect(screen.queryByRole('button', { name: /打开 ChatGPT/ })).toBeNull();
    const interrupted = screen.getByText('应用运行态', { selector: 'strong' }).closest('li');
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
