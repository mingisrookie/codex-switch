import { useEffect, useMemo, useRef, useState } from 'react';
import type { ManagedSessionInventory, ManagedSessionRecord } from './types';

type SessionFilter = 'all' | 'visible' | 'archived' | 'current' | 'shared';
type SessionSort = 'updated-desc' | 'updated-asc' | 'title-asc' | 'title-desc';

type SessionManagementPageProps = {
  inventory: ManagedSessionInventory;
  busy: boolean;
  syncDisabled: boolean;
  mutationDisabled: boolean;
  onSync: () => void;
  onDelete: (ids: string[], confirmed: boolean) => boolean | void | Promise<boolean | void>;
  onRestoreVisible: (ids: string[]) => boolean | void | Promise<boolean | void>;
};

const numberFormat = new Intl.NumberFormat('zh-CN');
const pageSize = 50;

export function SessionManagementPage({
  inventory,
  busy,
  syncDisabled,
  mutationDisabled,
  onSync,
  onDelete,
  onRestoreVisible,
}: SessionManagementPageProps) {
  const [filter, setFilter] = useState<SessionFilter>('all');
  const [query, setQuery] = useState('');
  const [sort, setSort] = useState<SessionSort>('updated-desc');
  const [page, setPage] = useState(1);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set());
  const selectAllRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    const availableIds = new Set(inventory.sessions.map((session) => session.id));
    setSelectedIds((current) => {
      const next = new Set(Array.from(current).filter((id) => availableIds.has(id)));
      return next.size === current.size ? current : next;
    });
  }, [inventory.sessions]);

  useEffect(() => setPage(1), [filter, query, sort, inventory.sessions]);

  const filteredSessions = useMemo(() => {
    const needle = query.trim().toLocaleLowerCase('zh-CN');
    return inventory.sessions
      .filter((session) => matchesFilter(session, filter))
      .filter((session) => !needle || searchableText(session).includes(needle))
      .slice()
      .sort((left, right) => compareSessions(left, right, sort));
  }, [filter, inventory.sessions, query, sort]);

  const pageCount = Math.max(1, Math.ceil(filteredSessions.length / pageSize));
  const currentPage = Math.min(page, pageCount);
  const sessions = filteredSessions.slice((currentPage - 1) * pageSize, currentPage * pageSize);
  const selectedSessions = inventory.sessions.filter((session) => selectedIds.has(session.id));
  const selectedArchived = selectedSessions.filter((session) => session.archived).length;
  const selectedUnarchived = selectedSessions.length - selectedArchived;
  const restoreIds = selectedSessions.filter(canRestoreVisible).map((session) => session.id);
  const visibleIds = sessions.map((session) => session.id);
  const allVisibleSelected = visibleIds.length > 0 && visibleIds.every((id) => selectedIds.has(id));
  const someVisibleSelected = visibleIds.some((id) => selectedIds.has(id));

  useEffect(() => {
    if (selectAllRef.current) {
      selectAllRef.current.indeterminate = someVisibleSelected && !allVisibleSelected;
    }
  }, [allVisibleSelected, someVisibleSelected]);

  function toggleSession(id: string) {
    setSelectedIds((current) => {
      const next = new Set(current);
      next.has(id) ? next.delete(id) : next.add(id);
      return next;
    });
  }

  function selectVisible() {
    setSelectedIds((current) => new Set([...current, ...visibleIds]));
  }

  function toggleVisibleSelection() {
    setSelectedIds((current) => {
      const next = new Set(current);
      for (const id of visibleIds) {
        if (allVisibleSelected) next.delete(id);
        else next.add(id);
      }
      return next;
    });
  }

  function invertVisibleSelection() {
    setSelectedIds((current) => {
      const next = new Set(current);
      for (const id of visibleIds) {
        next.has(id) ? next.delete(id) : next.add(id);
      }
      return next;
    });
  }

  function clearSelected() {
    setSelectedIds(new Set());
  }

  function handleBulkAction(action: string) {
    if (action === 'select-visible') selectVisible();
    if (action === 'invert-visible') invertVisibleSelection();
    if (action === 'clear') clearSelected();
  }

  async function deleteSelected() {
    const ids = Array.from(selectedIds);
    if (ids.length === 0) return;
    const selectedOnPage = visibleIds.filter((id) => selectedIds.has(id)).length;
    const selectedOnOtherPages = ids.length - selectedOnPage;
    const ok = window.confirm(
      `将硬删除 ${ids.length} 个会话（本页 ${selectedOnPage} 个，其他页 ${selectedOnOtherPages} 个）在当前 Codex Home 和 shared-sessions 中的副本。后端会先创建完整备份，确认继续？`,
    );
    if (!ok) return;
    if (ids.length > 10) {
      const confirmation = window.prompt(`批量硬删除风险较高，请输入“删除 ${ids.length}”继续。`, '');
      if (confirmation !== `删除 ${ids.length}`) return;
    }
    const succeeded = await onDelete(ids, true);
    if (succeeded === true) {
      setSelectedIds((current) => {
        const next = new Set(current);
        ids.forEach((id) => next.delete(id));
        return next;
      });
    }
  }

  async function restoreSelected() {
    if (restoreIds.length === 0) return;
    const succeeded = await onRestoreVisible(restoreIds);
    if (succeeded === true) {
      setSelectedIds((current) => {
        const next = new Set(current);
        restoreIds.forEach((id) => next.delete(id));
        return next;
      });
    }
  }

  return (
    <section className="session-management-page" aria-label="会话管理">
      <section className="hero-card session-hero">
        <div>
          <p className="eyebrow">当前 Codex Home + shared-sessions</p>
          <h1>会话管理</h1>
          <p className="lede">
            合并展示本机与共享池会话；归档会话默认只跳过同步，不自动清理。所有硬删除都需确认，
            超过 10 个会话还需输入确认文字。
          </p>
          <div className="hero-meta" aria-label="会话管理摘要">
            <span>合计：{numberFormat.format(inventory.totalCount)}</span>
            <span>已归档：{numberFormat.format(inventory.archivedCount)}</span>
            <span>可见：{numberFormat.format(inventory.totalCount - inventory.archivedCount)}</span>
            <span>已选：{numberFormat.format(selectedIds.size)}</span>
          </div>
        </div>
        <div className="hero-actions">
          <button className="primary-button" onClick={onSync} disabled={busy || syncDisabled}>立即同步</button>
        </div>
      </section>

      <section className="session-manager-grid">
        <aside className="detail-panel session-filter-panel">
          <p className="eyebrow">筛选</p>
          <label className="session-field">
            <span>搜索</span>
            <input
              aria-label="搜索会话"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="标题、ID、provider、路径"
            />
          </label>
          <label className="session-field">
            <span>排序</span>
            <select aria-label="会话排序" value={sort} onChange={(event) => setSort(event.target.value as SessionSort)}>
              <option value="updated-desc">最近更新</option>
              <option value="updated-asc">最早更新</option>
              <option value="title-asc">标题 A-Z</option>
              <option value="title-desc">标题 Z-A</option>
            </select>
          </label>
          <div className="filter-list" role="group" aria-label="会话筛选">
            <FilterButton label="全部" active={filter === 'all'} onClick={() => setFilter('all')} />
            <FilterButton label="未归档" active={filter === 'visible'} onClick={() => setFilter('visible')} />
            <FilterButton label="已归档" active={filter === 'archived'} onClick={() => setFilter('archived')} />
            <FilterButton label="本机" active={filter === 'current'} onClick={() => setFilter('current')} />
            <FilterButton label="共享池" active={filter === 'shared'} onClick={() => setFilter('shared')} />
          </div>
          <p className="safe-note">搜索作用于当前筛选；批量全选/反选只作用于本页，已选会话会跨页保留。每页最多显示 50 个会话。</p>
        </aside>

        <section className="session-table-card">
          <div className="session-selection-toolbar" aria-label="批量选择">
            <div className="selection-left">
              <label className="select-all-box">
                <input
                  ref={selectAllRef}
                  type="checkbox"
                  checked={allVisibleSelected}
                  onChange={toggleVisibleSelection}
                  disabled={busy || sessions.length === 0}
                  aria-label="全选本页"
                />
                <span>全选本页</span>
              </label>
              <button onClick={invertVisibleSelection} disabled={busy || sessions.length === 0}>反选本页</button>
            </div>
            <label className="bulk-select-field">
              <span>选择操作</span>
              <select
                className="session-bulk-select"
                defaultValue=""
                onChange={(event) => {
                  handleBulkAction(event.target.value);
                  event.target.value = '';
                }}
                disabled={busy || sessions.length === 0}
                aria-label="选择操作"
              >
                <option value="" disabled>批量选择</option>
                <option value="select-visible">全选本页</option>
                <option value="invert-visible">反选本页</option>
                <option value="clear">清空选择</option>
              </select>
            </label>
          </div>
          <div className="session-table" role="table" aria-label="会话列表">
            <div className="session-table-head" role="row">
              <span role="columnheader" aria-label="选择" />
              <span role="columnheader">会话 / 路径</span>
              <span role="columnheader">Provider</span>
              <span role="columnheader">状态</span>
              <span role="columnheader">来源</span>
              <span role="columnheader">更新时间</span>
            </div>
            {sessions.length === 0 ? (
              <div role="row"><p className="empty-state" role="cell">当前筛选下没有会话。</p></div>
            ) : sessions.map((session) => (
              <SessionRow
                key={session.id}
                session={session}
                selected={selectedIds.has(session.id)}
                disabled={busy}
                onToggle={() => toggleSession(session.id)}
              />
            ))}
          </div>
          <div className="session-pagination" aria-label="会话分页">
            <button onClick={() => setPage((value) => Math.max(1, value - 1))} disabled={busy || currentPage === 1}>上一页</button>
            <span>第 {currentPage} / {pageCount} 页</span>
            <button onClick={() => setPage((value) => Math.min(pageCount, value + 1))} disabled={busy || currentPage === pageCount}>下一页</button>
          </div>
        </section>

        <aside className="detail-panel selected-session-panel">
          <div className="card-title-row">
            <span className="card-icon">🗂️</span>
            <div><p className="eyebrow">所选会话</p><h2>{numberFormat.format(selectedIds.size)} 个</h2></div>
          </div>
          <dl className="compact-meta">
            <div><dt>未归档</dt><dd>{numberFormat.format(selectedUnarchived)}</dd></div>
            <div><dt>已归档</dt><dd>{numberFormat.format(selectedArchived)}</dd></div>
            <div><dt>可恢复</dt><dd>{numberFormat.format(restoreIds.length)}</dd></div>
          </dl>
          <div className="detail-actions">
            <button onClick={() => void restoreSelected()} disabled={busy || mutationDisabled || restoreIds.length === 0}>恢复可见</button>
            <button className="danger" onClick={() => void deleteSelected()} disabled={busy || mutationDisabled || selectedIds.size === 0}>删除所选</button>
            <button onClick={clearSelected} disabled={busy || selectedIds.size === 0}>清空选择</button>
          </div>
          <p className="safe-note">恢复只处理当前 Home 中已归档的会话；共享池独有或未归档会话不会误写。</p>
        </aside>
      </section>
    </section>
  );
}

function SessionRow({ session, selected, disabled, onToggle }: { session: ManagedSessionRecord; selected: boolean; disabled: boolean; onToggle: () => void }) {
  const displayTitle = session.title || session.preview || '未命名会话';
  const path = effectivePath(session);
  return (
    <label className={`session-row ${selected ? 'selected' : ''}`} title={session.id} role="row">
      <span role="cell"><input type="checkbox" checked={selected} disabled={disabled} onChange={onToggle} aria-label={`选择 ${session.id}：${displayTitle}`} /></span>
      <span className="session-title-cell" role="cell">
        <strong title={displayTitle}>{displayTitle}</strong>
        <small className="session-path" title={path}>{path}</small>
      </span>
      <span className="session-provider" title={session.modelProvider ?? '未知'} role="cell">{session.modelProvider ?? '未知'}</span>
      <span className={`pill ${session.archived ? 'orange' : 'teal'}`} role="cell">{session.archived ? '已归档' : '未归档'}</span>
      <span role="cell">{sourceLabel(session.scope)}</span>
      <span role="cell">{formatTime(session.updatedAtMs ?? session.updatedAt)}</span>
    </label>
  );
}

function FilterButton({ label, active, onClick }: { label: string; active: boolean; onClick: () => void }) {
  return <button className={active ? 'active' : ''} onClick={onClick} aria-pressed={active}>{label}</button>;
}

function matchesFilter(session: ManagedSessionRecord, filter: SessionFilter) {
  if (filter === 'visible') return !session.archived;
  if (filter === 'archived') return session.archived;
  if (filter === 'current') return session.scope === 'current' || session.scope === 'both';
  if (filter === 'shared') return session.scope === 'shared' || session.scope === 'both';
  return true;
}

function compareSessions(left: ManagedSessionRecord, right: ManagedSessionRecord, sort: SessionSort) {
  const leftTitle = left.title || left.preview || '';
  const rightTitle = right.title || right.preview || '';
  if (sort === 'title-asc') return leftTitle.localeCompare(rightTitle, 'zh-CN') || left.id.localeCompare(right.id);
  if (sort === 'title-desc') return rightTitle.localeCompare(leftTitle, 'zh-CN') || right.id.localeCompare(left.id);
  const leftTime = left.updatedAtMs ?? left.updatedAt ?? 0;
  const rightTime = right.updatedAtMs ?? right.updatedAt ?? 0;
  return sort === 'updated-asc' ? leftTime - rightTime : rightTime - leftTime;
}

function searchableText(session: ManagedSessionRecord) {
  return [
    session.id,
    session.title,
    session.preview,
    session.modelProvider,
    session.current?.sessionFile,
    session.current?.rolloutPath,
    session.shared?.sessionFile,
    session.shared?.rolloutPath,
  ]
    .filter(Boolean)
    .join('\n')
    .toLocaleLowerCase('zh-CN');
}

function canRestoreVisible(session: ManagedSessionRecord) {
  return Boolean(session.archived && session.current?.archived);
}

function effectivePath(session: ManagedSessionRecord) {
  return session.current?.sessionFile || session.current?.rolloutPath || session.shared?.sessionFile || session.shared?.rolloutPath || '无 JSONL 路径';
}

function sourceLabel(scope: ManagedSessionRecord['scope']) {
  if (scope === 'current') return '本机';
  if (scope === 'shared') return '共享池';
  if (scope === 'both') return '两边都有';
  return '未知';
}

function formatTime(value: number | null) {
  if (!value) return '未知';
  const millis = value > 10_000_000_000 ? value : value * 1000;
  return new Date(millis).toLocaleString('zh-CN', { hour12: false });
}
