import { useEffect, useMemo, useRef, useState } from 'react';
import {
  ArchiveRestore,
  ChevronLeft,
  ChevronRight,
  FolderArchive,
  RefreshCw,
  Repeat2,
  X,
} from 'lucide-react';
import type {
  ManagedSessionInventory,
  ManagedSessionRecord,
  MobileContinuityItemStatus,
  MobileContinuityStatus,
} from './types';

import {
  createSessionIndex,
  filterSessionIndex,
  sessionTimestampMs,
  sortSessionIndex,
  type SessionScopeFilter,
  type SessionSort,
} from './sessionListModel';

type SessionFilter = SessionScopeFilter | 'selected';

type SessionManagementPageProps = {
  inventory: ManagedSessionInventory;
  busy: boolean;
  syncDisabled: boolean;
  mutationDisabled: boolean;
  onSync: () => void;
  onRestoreVisible: (ids: string[]) => boolean | void | Promise<boolean | void>;
  mobileContinuity?: MobileContinuityStatus | null;
  onPublishMobile?: (threadId: string) => boolean | void | Promise<boolean | void>;
  mobilePublishDisabled?: boolean;
};

const numberFormat = new Intl.NumberFormat('zh-CN');
const timeFormat = new Intl.DateTimeFormat('zh-CN', {
  year: 'numeric', month: 'numeric', day: 'numeric',
  hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false,
});
const pageSize = 50;

export function SessionManagementPage({
  inventory,
  busy,
  syncDisabled,
  mutationDisabled,
  onSync,
  onRestoreVisible,
  mobileContinuity = null,
  onPublishMobile = () => undefined,
  mobilePublishDisabled = false,
}: SessionManagementPageProps) {
  const [filter, setFilter] = useState<SessionFilter>('all');
  const [query, setQuery] = useState('');
  const [sort, setSort] = useState<SessionSort>('updated-desc');
  const [page, setPage] = useState(1);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set());
  const selectAllRef = useRef<HTMLInputElement>(null);
  const selectionHeadingRef = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    const availableIds = new Set(inventory.sessions.map((session) => session.id));
    setSelectedIds((current) => {
      const next = new Set(Array.from(current).filter((id) => availableIds.has(id)));
      return next.size === current.size ? current : next;
    });
  }, [inventory.sessions]);

  useEffect(() => setPage(1), [filter, query, sort]);

  // Build the corpus only when inventory changes; keystrokes never rebuild or re-sort it.
  const searchIndex = useMemo(() => createSessionIndex(inventory.sessions), [inventory.sessions]);
  const sortedIndex = useMemo(() => sortSessionIndex(searchIndex, sort), [searchIndex, sort]);
  const matchingSessions = useMemo(
    () => filterSessionIndex(sortedIndex, filter === 'selected' ? 'all' : filter, query),
    [sortedIndex, filter, query],
  );
  const filteredSessions = useMemo(
    () => filter === 'selected' ? matchingSessions.filter((session) => selectedIds.has(session.id)) : matchingSessions,
    [filter, matchingSessions, selectedIds],
  );
  const continuityByThread = useMemo(
    () => new Map(mobileContinuity?.items.map((item) => [item.threadId, item.status]) ?? []),
    [mobileContinuity?.items],
  );

  const pageCount = Math.max(1, Math.ceil(filteredSessions.length / pageSize));
  const currentPage = Math.min(page, pageCount);
  const sessions = filteredSessions.slice((currentPage - 1) * pageSize, currentPage * pageSize);
  const selectedSessions = useMemo(
    () => inventory.sessions.filter((session) => selectedIds.has(session.id)),
    [inventory.sessions, selectedIds],
  );
  const selectedArchived = selectedSessions.filter((session) => session.archived).length;
  const selectedUnarchived = selectedSessions.length - selectedArchived;
  const restoreIds = selectedSessions.filter(canRestoreVisible).map((session) => session.id);
  const visibleIds = sessions.map((session) => session.id);
  const allVisibleSelected = visibleIds.length > 0 && visibleIds.every((id) => selectedIds.has(id));
  const someVisibleSelected = visibleIds.some((id) => selectedIds.has(id));
  const offPageSelectedCount = selectedSessions.length - visibleIds.filter((id) => selectedIds.has(id)).length;

  // Clamp the stored page as well: a later refresh must not resurrect an obsolete page.
  useEffect(() => setPage((current) => Math.min(current, pageCount)), [pageCount]);

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

  function showSelected() {
    setQuery('');
    setFilter('selected');
    setPage(1);
  }

  function handleBulkAction(action: string) {
    if (action === 'select-visible') selectVisible();
    if (action === 'invert-visible') invertVisibleSelection();
    if (action === 'clear') clearSelected();
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
          <p className="eyebrow">ChatGPT 数据目录 + shared-sessions</p>
          <h1>会话管理</h1>
          <p className="lede">本机与共享池的统一会话视图。</p>
          <div className="hero-meta" aria-label="会话管理摘要">
            <span>合计：{numberFormat.format(inventory.totalCount)}</span>
            <span>已归档：{numberFormat.format(inventory.archivedCount)}</span>
            <span>可见：{numberFormat.format(inventory.totalCount - inventory.archivedCount)}</span>
            <span>已选：{numberFormat.format(selectedIds.size)}</span>
          </div>
        </div>
        <div className="hero-actions">
          <button className="primary-button" onClick={onSync} disabled={busy || syncDisabled}>
            <RefreshCw className="button-icon" aria-hidden="true" />
            会话合并与修复
          </button>
        </div>
      </section>

      <section className="session-manager-grid">
        <aside className="detail-panel session-filter-panel">
          <p className="eyebrow">筛选</p>
          <label className="session-field">
            <span>搜索</span>
            <input
              type="search"
              aria-label="搜索会话"
              value={query}
              onChange={(event) => setQuery(event.target.value)}
              placeholder="标题、ID、provider、路径"
            />
          </label>
          <button className="ghost-button inline" onClick={() => setQuery('')} disabled={!query}>
            <X className="button-icon" aria-hidden="true" />
            清空搜索
          </button>
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
            <FilterButton label="只看已选" active={filter === 'selected'} onClick={() => setFilter('selected')} />
          </div>
          <p className="safe-note">每页 50 个会话，选择状态跨页保留。</p>
        </aside>

        <section
          className="session-table-card"
          role="region"
          aria-label="会话表格，可横向滚动"
          tabIndex={0}
        >
          <p className="session-list-summary" role="status" aria-live="polite" aria-atomic="true">
            匹配 {numberFormat.format(filteredSessions.length)} / {numberFormat.format(inventory.sessions.length)} 个会话
            {filteredSessions.length > 0 && ` · 本页 ${(currentPage - 1) * pageSize + 1}–${Math.min(currentPage * pageSize, filteredSessions.length)}`}
          </p>
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
              <button onClick={invertVisibleSelection} disabled={busy || sessions.length === 0}>
                <Repeat2 className="button-icon" aria-hidden="true" />
                反选本页
              </button>
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
                disabled={busy || (sessions.length === 0 && selectedIds.size === 0)}
                aria-label="选择操作"
              >
                <option value="" disabled>批量选择</option>
                <option value="select-visible" disabled={sessions.length === 0}>全选本页</option>
                <option value="invert-visible" disabled={sessions.length === 0}>反选本页</option>
                <option value="clear" disabled={selectedIds.size === 0}>清空选择</option>
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
              <span role="columnheader">Remote</span>
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
                continuityStatus={continuityByThread.get(session.id) ?? null}
                onPublish={() => void onPublishMobile(session.id)}
                mobilePublishDisabled={mobilePublishDisabled}
              />
            ))}
          </div>
          <div className="session-pagination" aria-label="会话分页">
            <button onClick={() => setPage((value) => Math.max(1, value - 1))} disabled={busy || currentPage === 1}>
              <ChevronLeft className="button-icon" aria-hidden="true" />
              上一页
            </button>
            <span>第 {currentPage} / {pageCount} 页</span>
            <button onClick={() => setPage((value) => Math.min(pageCount, value + 1))} disabled={busy || currentPage === pageCount}>
              下一页
              <ChevronRight className="button-icon" aria-hidden="true" />
            </button>
          </div>
        </section>

        <aside className="detail-panel selected-session-panel">
          <div className="card-title-row">
            <span className="section-icon"><FolderArchive aria-hidden="true" /></span>
            <div><p className="eyebrow">所选会话</p><h2 ref={selectionHeadingRef} tabIndex={-1}>{numberFormat.format(selectedIds.size)} 个</h2></div>
          </div>
          <dl className="compact-meta">
            <div><dt>未归档</dt><dd>{numberFormat.format(selectedUnarchived)}</dd></div>
            <div><dt>已归档</dt><dd>{numberFormat.format(selectedArchived)}</dd></div>
            <div><dt>可恢复</dt><dd>{numberFormat.format(restoreIds.length)}</dd></div>
          </dl>
          {offPageSelectedCount > 0 && (
            <div className="session-selection-note">
              <p className="safe-note" role="status">
                另有 {numberFormat.format(offPageSelectedCount)} 个已选会话不在本页；恢复可见仍包含其中可恢复的会话。
              </p>
              <button className="ghost-button inline" onClick={showSelected}>查看全部已选</button>
            </div>
          )}
          <div className="detail-actions">
            <button onClick={() => void restoreSelected()} disabled={busy || mutationDisabled || restoreIds.length === 0}>
              <ArchiveRestore className="button-icon" aria-hidden="true" />
              恢复可见
            </button>
            <button onClick={clearSelected} disabled={busy || selectedIds.size === 0}>
              <X className="button-icon" aria-hidden="true" />
              清空选择
            </button>
          </div>
          <p className="safe-note">
            恢复范围：当前 Home 中已归档的会话。v0.3 不提供直接硬删除；历史副本只由全局引用证明的安全清理回收。
          </p>
        </aside>
      </section>
    </section>
  );
}

function SessionRow({
  session,
  selected,
  disabled,
  onToggle,
  continuityStatus,
  onPublish,
  mobilePublishDisabled,
}: {
  session: ManagedSessionRecord;
  selected: boolean;
  disabled: boolean;
  onToggle: () => void;
  continuityStatus: MobileContinuityItemStatus | null;
  onPublish: () => void;
  mobilePublishDisabled: boolean;
}) {
  const displayTitle = session.title || session.preview || '未命名会话';
  const path = effectivePath(session);
  const legacyRelay = !session.archived
    && session.modelProvider === 'openai_custom'
    && continuityStatus === null;
  return (
    <div className={`session-row ${selected ? 'selected' : ''}`} title={session.id} role="row">
      <span role="cell"><input type="checkbox" checked={selected} disabled={disabled} onChange={onToggle} aria-label={`选择 ${session.id}：${displayTitle}`} /></span>
      <span className="session-title-cell" role="cell">
        <strong title={displayTitle}>{displayTitle}</strong>
        <small className="session-path" title={path}>{path}</small>
      </span>
      <span className="session-provider" title={session.modelProvider ?? '未知'} role="cell">{session.modelProvider ?? '未知'}</span>
      <span className={`pill ${session.archived ? 'orange' : 'teal'}`} role="cell">{session.archived ? '已归档' : '未归档'}</span>
      <span role="cell">{sourceLabel(session.scope)}</span>
      <span role="cell">{formatTime(sessionTimestampMs(session))}</span>
      <span className="session-remote-action" role="cell">
        {continuityStatus ? (
          <span className={`pill ${continuityStatus === 'remotePublished' ? 'teal' : 'orange'}`}>
            {continuityStatusLabel(continuityStatus)}
          </span>
        ) : legacyRelay ? (
          <button
            className="ghost-button inline"
            disabled={disabled || mobilePublishDisabled}
            title={mobilePublishDisabled ? '请先切回 OpenAI 官方请求端' : undefined}
            onClick={onPublish}
          >
            <Repeat2 className="button-icon" aria-hidden="true" />
            同步此会话
          </button>
        ) : <span className="muted-dash">—</span>}
      </span>
    </div>
  );
}

function continuityStatusLabel(status: MobileContinuityItemStatus) {
  const labels: Record<MobileContinuityItemStatus, string> = {
    queued: '待发布',
    publishing: '发布中',
    remotePublished: 'Account 视图',
    partial: '兼容状态',
    conflict: '冲突待处理',
    retrying: '重试中',
    needsManual: '需手动处理',
    paused: '已暂停',
  };
  return labels[status];
}

function FilterButton({ label, active, onClick }: { label: string; active: boolean; onClick: () => void }) {
  return <button className={active ? 'active' : ''} onClick={onClick} aria-pressed={active}>{label}</button>;
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

function formatTime(millis: number | null) {
  return millis === null ? '未知' : timeFormat.format(millis);
}
