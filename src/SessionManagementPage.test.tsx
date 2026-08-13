import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { SessionManagementPage } from './SessionManagementPage';
import type { ManagedSessionInventory, ManagedSessionRecord } from './types';

function session(index: number, overrides: Partial<ManagedSessionRecord> = {}): ManagedSessionRecord {
  const id = `thread-${String(index).padStart(2, '0')}`;
  return {
    id,
    title: `会话 ${String(index).padStart(2, '0')}`,
    preview: null,
    modelProvider: index % 2 ? 'openai' : 'openai_custom',
    updatedAt: index,
    updatedAtMs: index * 1000,
    archived: false,
    archivedAt: null,
    scope: 'both',
    current: {
      home: 'C:\\Users\\alice\\.codex',
      rolloutPath: `C:\\Users\\alice\\.codex\\sessions\\${id}.jsonl`,
      sessionFile: `C:\\Users\\alice\\.codex\\sessions\\${id}.jsonl`,
      archived: false,
      archivedAt: null,
      updatedAt: index,
      updatedAtMs: index * 1000,
    },
    shared: null,
    ...overrides,
  };
}

function inventory(sessions: ManagedSessionRecord[]): ManagedSessionInventory {
  return {
    currentHome: 'C:\\Users\\alice\\.codex',
    sharedHome: 'C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\shared-sessions',
    totalCount: sessions.length,
    archivedCount: sessions.filter((item) => item.archived).length,
    sessions,
  };
}

function renderPage(
  sessions: ManagedSessionRecord[],
  onRestoreVisible = vi.fn(),
) {
  return render(
    <SessionManagementPage
      inventory={inventory(sessions)}
      busy={false}
      syncDisabled={false}
      mutationDisabled={false}
      onSync={vi.fn()}
      onRestoreVisible={onRestoreVisible}
    />,
  );
}

describe('SessionManagementPage', () => {
  it('searches, sorts, and paginates sessions in pages of 50', () => {
    const sessions = Array.from({ length: 55 }, (_, index) => session(index + 1));
    renderPage(sessions);

    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(50);
    expect(screen.getByText('第 1 / 2 页')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '下一页' }));
    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(5);

    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: '会话 03' } });
    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(1);
    expect(screen.getByText('会话 03')).toBeTruthy();

    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: '' } });
    fireEvent.change(screen.getByLabelText('会话排序'), { target: { value: 'title-desc' } });
    expect(screen.getAllByLabelText(/^选择 thread-/)[0].getAttribute('aria-label')).toBe('选择 thread-55：会话 55');
  });

  it('exposes the horizontally scrollable session table as a keyboard focusable region', () => {
    renderPage([session(1)]);

    const region = screen.getByRole('region', { name: '会话表格，可横向滚动' });
    expect(region.getAttribute('tabindex')).toBe('0');
    expect(within(region).getByRole('table', { name: '会话列表' })).toBeTruthy();
  });

  it('offers legacy relay sessions an Account view action and renders typed status', () => {
    const onPublishMobile = vi.fn();
    render(
      <SessionManagementPage
        inventory={inventory([session(2), session(4)])}
        busy={false}
        syncDisabled={false}
        mutationDisabled={false}
        onSync={vi.fn()}
        onRestoreVisible={vi.fn()}
        mobileContinuity={{
          enabled: true,
          noticePending: false,
          initializedAtMs: 1,
          queued: 0,
          publishing: 0,
          remotePublished: 1,
          partial: 0,
          conflict: 0,
          needsManual: 0,
          items: [{
            threadId: 'thread-04',
            status: 'remotePublished',
            attempts: 1,
            nextRetryAtMs: null,
            updatedAtMs: 2,
            failureCategory: null,
            sourceFingerprint: null,
          }],
        }}
        onPublishMobile={onPublishMobile}
      />,
    );

    fireEvent.click(screen.getByRole('button', { name: '同步此会话' }));

    expect(onPublishMobile).toHaveBeenCalledWith('thread-02');
    expect(screen.getByText('Account 视图')).toBeTruthy();
  });

  it('keeps selections across pages and exposes the partial-page indeterminate state', () => {
    const sessions = Array.from({ length: 55 }, (_, index) => session(index + 1));
    renderPage(sessions);

    fireEvent.click(screen.getByLabelText(/^选择 thread-55/));
    const selectPage = screen.getByLabelText('全选本页') as HTMLInputElement;
    expect(selectPage.indeterminate).toBe(true);
    fireEvent.click(screen.getByRole('button', { name: '下一页' }));
    fireEvent.click(screen.getByLabelText('全选本页'));

    expect(screen.getByText('已选：6')).toBeTruthy();
  });

  it('removes selections that no longer exist after an inventory refresh', async () => {
    const view = renderPage([session(1)]);
    fireEvent.click(screen.getByLabelText(/^选择 thread-01/));
    expect(screen.getByText('已选：1')).toBeTruthy();

    view.rerender(
      <SessionManagementPage
        inventory={inventory([])}
        busy={false}
        syncDisabled={false}
        mutationDisabled={false}
        onSync={vi.fn()}
        onRestoreVisible={vi.fn()}
      />,
    );

    await waitFor(() => expect(screen.getByText('已选：0')).toBeTruthy());
  });

  it('does not expose the retired hard-delete action', () => {
    renderPage([session(1)]);
    fireEvent.click(screen.getByLabelText(/^选择 thread-01/));

    expect(screen.queryByRole('button', { name: /删除|硬删除/ })).toBeNull();
    expect(screen.getByText(/v0\.3 不提供直接硬删除/)).toBeTruthy();
  });

  it('restores only archived sessions that still exist in the current home', () => {
    const onRestore = vi.fn();
    renderPage([
      session(1, {
        archived: true,
        archivedAt: 1000,
        current: { ...session(1).current!, archived: true, archivedAt: 1000 },
      }),
      session(2, { archived: true, archivedAt: 2000, current: null, scope: 'shared' }),
      session(3),
    ], onRestore);

    fireEvent.click(screen.getByLabelText('全选本页'));
    fireEvent.click(screen.getByRole('button', { name: '恢复可见' }));

    expect(onRestore).toHaveBeenCalledWith(['thread-01']);
  });

  it('shows provider and the effective rollout path', () => {
    renderPage([session(1)]);

    expect(screen.getByText('openai')).toBeTruthy();
    expect(screen.getByText(/thread-01\.jsonl/)).toBeTruthy();
  });

  it('searches both current and shared paths when a session exists in both roots', () => {
    renderPage([session(1, {
      shared: {
        home: 'D:\\shared', rolloutPath: 'D:\\shared\\sessions\\shared-needle.jsonl',
        sessionFile: 'D:\\shared\\sessions\\shared-needle.jsonl', archived: false,
        archivedAt: null, updatedAt: 1, updatedAtMs: 1000,
      },
    })]);

    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: 'shared-needle' } });
    expect(screen.getByLabelText(/^选择 thread-01/)).toBeTruthy();
  });

  it('disables row selection while a mutation is in flight', () => {
    render(
      <SessionManagementPage
        inventory={inventory([session(1)])}
        busy
        syncDisabled={false}
        mutationDisabled={false}
        onSync={vi.fn()}
        onRestoreVisible={vi.fn()}
      />,
    );

    expect((screen.getByLabelText(/^选择 thread-01/) as HTMLInputElement).disabled).toBe(true);
  });
});
