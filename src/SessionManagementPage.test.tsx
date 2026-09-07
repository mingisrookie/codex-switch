import { fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { SessionManagementPage } from './SessionManagementPage';
import * as listModel from './sessionListModel';
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


describe('session list navigation and selection regressions', () => {
  function pageWith(sessions: ManagedSessionRecord[], busy = false, onRestoreVisible = vi.fn()) {
    return <SessionManagementPage inventory={inventory(sessions)} busy={busy} syncDisabled={false}
      mutationDisabled={false} onSync={vi.fn()} onRestoreVisible={onRestoreVisible} />;
  }

  it('keeps the current page across inventory refreshes and clamps after shrink without bouncing back', () => {
    const sessions = Array.from({ length: 105 }, (_, i) => session(i + 1));
    const view = render(pageWith(sessions));
    fireEvent.click(screen.getByRole('button', { name: '下一页' }));
    fireEvent.click(screen.getByRole('button', { name: '下一页' }));
    view.rerender(pageWith([...sessions]));
    expect(screen.getByText('第 3 / 3 页')).toBeTruthy();
    view.rerender(pageWith(sessions.slice(0, 55)));
    expect(screen.getByText('第 2 / 2 页')).toBeTruthy();
    view.rerender(pageWith(sessions));
    expect(screen.getByText('第 2 / 3 页')).toBeTruthy();
  });

  it('resets to the first page only when the user changes query, filter or sort', () => {
    const sessions = Array.from({ length: 105 }, (_, i) => session(i + 1));
    renderPage(sessions);
    for (const change of [
      () => fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: '会话' } }),
      () => fireEvent.click(screen.getByRole('button', { name: '未归档' })),
      () => fireEvent.change(screen.getByLabelText('会话排序'), { target: { value: 'updated-asc' } }),
    ]) {
      fireEvent.click(screen.getByRole('button', { name: '下一页' }));
      change();
      expect(screen.getByText('第 1 / 3 页')).toBeTruthy();
    }
  });

  it('shows result counts and clears the search without clearing selections', () => {
    renderPage([session(1), session(2)]);
    fireEvent.click(screen.getByLabelText(/^选择 thread-02/));
    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: 'thread-01' } });
    expect(screen.getByText(/匹配 1 \/ 2 个会话/)).toBeTruthy();
    expect(screen.getByText(/另有 1 个已选会话不在本页/)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '清空搜索' }));
    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(2);
    expect(screen.getByText('已选：1')).toBeTruthy();
  });

  it('shows cross-page selections and lets the user inspect all selected rows', () => {
    renderPage(Array.from({ length: 55 }, (_, i) => session(i + 1)));
    fireEvent.click(screen.getByLabelText(/^选择 thread-55/));
    fireEvent.click(screen.getByRole('button', { name: '下一页' }));
    fireEvent.click(screen.getByLabelText(/^选择 thread-01/));
    expect(screen.getByText(/另有 1 个已选会话不在本页/)).toBeTruthy();
    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: 'missing' } });
    fireEvent.click(screen.getByRole('button', { name: '查看全部已选' }));
    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(2);
    expect(screen.getByRole('button', { name: '只看已选' }).getAttribute('aria-pressed')).toBe('true');
    expect((screen.getByLabelText('搜索会话') as HTMLInputElement).value).toBe('');
    fireEvent.click(screen.getByLabelText(/^选择 thread-55/));
    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(1);
  });

  it('allows clearing hidden selections from the toolbar when the search has no results', () => {
    renderPage([session(1)]);
    fireEvent.click(screen.getByLabelText(/^选择 thread-01/));
    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: 'missing' } });
    const bulk = screen.getByLabelText('选择操作') as HTMLSelectElement;
    expect(bulk.disabled).toBe(false);
    fireEvent.change(bulk, { target: { value: 'clear' } });
    expect(screen.getByText('已选：0')).toBeTruthy();
    expect(screen.getByText(/匹配 0 \/ 1 个会话/)).toBeTruthy();
  });

  it('preserves page-local selection semantics in the selected-only view', () => {
    renderPage(Array.from({ length: 55 }, (_, i) => session(i + 1)));
    fireEvent.click(screen.getByLabelText('全选本页'));
    fireEvent.click(screen.getByRole('button', { name: '下一页' }));
    fireEvent.click(screen.getByLabelText('全选本页'));
    fireEvent.click(screen.getByRole('button', { name: '只看已选' }));
    fireEvent.click(screen.getByRole('button', { name: '下一页' }));
    fireEvent.click(screen.getByRole('button', { name: '反选本页' }));
    expect(screen.getByText('已选：50')).toBeTruthy();
    expect(screen.getByText('第 1 / 1 页')).toBeTruthy();
    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(50);
  });

  it('reconciles removed selected rows while the selected-only filter is active', () => {
    const view = render(pageWith([session(1), session(2)]));
    fireEvent.click(screen.getByLabelText('全选本页'));
    fireEvent.click(screen.getByRole('button', { name: '只看已选' }));
    view.rerender(pageWith([session(2)]));
    expect(screen.getByText('已选：1')).toBeTruthy();
    expect(screen.getAllByLabelText(/^选择 thread-/)).toHaveLength(1);
  });

  it('does not rebuild the search index or repeat sorting for keystrokes and selection changes', () => {
    const indexSpy = vi.spyOn(listModel, 'createSessionIndex');
    const sortSpy = vi.spyOn(listModel, 'sortSessionIndex');
    try {
      renderPage([session(1), session(2)]);
      const indexCalls = indexSpy.mock.calls.length;
      const sortCalls = sortSpy.mock.calls.length;
      fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: 'thread' } });
      fireEvent.click(screen.getByLabelText(/^选择 thread-01/));
      expect(indexSpy).toHaveBeenCalledTimes(indexCalls);
      expect(sortSpy).toHaveBeenCalledTimes(sortCalls);
      fireEvent.change(screen.getByLabelText('会话排序'), { target: { value: 'title-asc' } });
      expect(indexSpy).toHaveBeenCalledTimes(indexCalls);
      expect(sortSpy).toHaveBeenCalledTimes(sortCalls + 1);
    } finally {
      indexSpy.mockRestore();
      sortSpy.mockRestore();
    }
  });

  it('sorts and displays a mixed-unit timestamp consistently', () => {
    renderPage([
      session(1, { updatedAtMs: 1_700_000_000_000 }),
      session(2, { updatedAtMs: null, updatedAt: 1_800_000_000 }),
    ]);
    expect(screen.getAllByLabelText(/^选择 thread-/)[0].getAttribute('aria-label')).toContain('thread-02');
    expect(screen.getByText(new Intl.DateTimeFormat('zh-CN', {
      year: 'numeric', month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false,
    }).format(1_800_000_000_000))).toBeTruthy();
  });

  it('restores all eligible selections, including off-page rows, and clears only successful selections', async () => {
    const archived = (i: number) => session(i, { archived: true, current: { ...session(i).current!, archived: true } });
    const onRestore = vi.fn().mockResolvedValueOnce(false).mockResolvedValueOnce(true);
    renderPage([archived(1), archived(2), session(3)], onRestore);
    fireEvent.click(screen.getByLabelText('全选本页'));
    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: 'thread-03' } });
    fireEvent.click(screen.getByRole('button', { name: '恢复可见' }));
    await waitFor(() => expect(onRestore).toHaveBeenCalledTimes(1));
    expect(onRestore).toHaveBeenLastCalledWith(['thread-01', 'thread-02']);
    expect(screen.getByText('已选：3')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: '恢复可见' }));
    await waitFor(() => expect(screen.getByText('已选：1')).toBeTruthy());
  });

  it('keeps empty-result clear and restore actions disabled during a mutation', () => {
    const sessions = [session(1, { archived: true, current: { ...session(1).current!, archived: true } })];
    const view = render(pageWith(sessions));
    fireEvent.click(screen.getByLabelText('全选本页'));
    fireEvent.change(screen.getByLabelText('搜索会话'), { target: { value: 'missing' } });
    view.rerender(pageWith(sessions, true));
    expect((screen.getByLabelText('选择操作') as HTMLSelectElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '清空选择' }) as HTMLButtonElement).disabled).toBe(true);
    expect((screen.getByRole('button', { name: '恢复可见' }) as HTMLButtonElement).disabled).toBe(true);
  });
});
