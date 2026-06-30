import { useMemo, useState } from 'react';
import type { ManagedSessionInventory, ManagedSessionRecord } from './types';

type SessionFilter = 'all' | 'visible' | 'archived' | 'current' | 'shared';

type SessionManagementPageProps = {
  inventory: ManagedSessionInventory;
  busy: boolean;
  onSync: () => void;
  onDelete: (ids: string[], confirmUnarchived: boolean) => void;
  onRestoreVisible: (ids: string[]) => void;
};

const numberFormat = new Intl.NumberFormat('zh-CN');

export function SessionManagementPage({
  inventory,
  busy,
  onSync,
  onDelete,
  onRestoreVisible,
}: SessionManagementPageProps) {
  const [filter, setFilter] = useState<SessionFilter>('all');
  const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set());

  const sessions = useMemo(
    () => inventory.sessions.filter((session) => matchesFilter(session, filter)),
    [filter, inventory.sessions],
  );
  const selectedSessions = inventory.sessions.filter((session) => selectedIds.has(session.id));
  const selectedArchived = selectedSessions.filter((session) => session.archived).length;
  const selectedUnarchived = selectedSessions.length - selectedArchived;

  function toggleSession(id: string) {
    setSelectedIds((current) => {
      const next = new Set(current);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }

  function selectVisible() {
    setSelectedIds(new Set(sessions.map((session) => session.id)));
  }

  function clearSelected() {
    setSelectedIds(new Set());
  }

  function deleteSelected() {
    const ids = Array.from(selectedIds);
    if (ids.length === 0) return;
    const needsConfirmation = selectedUnarchived > 0;
    if (needsConfirmation) {
      const ok = window.confirm(
        `已选择 ${ids.length} 个会话，其中 ${selectedUnarchived} 个未归档。删除会同时硬删除当前 Codex Home 和 shared-sessions 的副本，并已由后端先备份。确认继续？`,
      );
      if (!ok) return;
    }
    onDelete(ids, needsConfirmation);
  }

  function restoreSelected() {
    const ids = Array.from(selectedIds);
    if (ids.length === 0) return;
    onRestoreVisible(ids);
  }

  return (
    <section className="session-management-page" aria-label="会话管理">
      <section className="hero-card session-hero">
        <div>
          <p className="eyebrow">当前 Codex Home + shared-sessions</p>
          <h1>会话管理</h1>
          <p className="lede">
            合并展示本机与共享池会话；归档会话默认只跳过同步，不自动清理。删除为安全硬删除：
            已归档直接备份后删除，未归档需要二次确认。
          </p>
          <div className="hero-meta" aria-label="会话管理摘要">
            <span>合计：{numberFormat.format(inventory.totalCount)}</span>
            <span>已归档：{numberFormat.format(inventory.archivedCount)}</span>
            <span>可见：{numberFormat.format(inventory.totalCount - inventory.archivedCount)}</span>
            <span>已选：{numberFormat.format(selectedIds.size)}</span>
          </div>
        </div>
        <div className="hero-actions">
          <button className="primary-button" onClick={onSync} disabled={busy}>
            立即同步
          </button>
          <button className="ghost-button inline" onClick={selectVisible} disabled={busy || sessions.length === 0}>
            选择当前列表
          </button>
        </div>
      </section>

      <section className="session-manager-grid">
        <aside className="detail-panel session-filter-panel">
          <p className="eyebrow">筛选</p>
          <div className="filter-list" role="tablist" aria-label="会话筛选">
            <FilterButton label="全部" active={filter === 'all'} onClick={() => setFilter('all')} />
            <FilterButton label="未归档" active={filter === 'visible'} onClick={() => setFilter('visible')} />
            <FilterButton label="已归档" active={filter === 'archived'} onClick={() => setFilter('archived')} />
            <FilterButton label="本机" active={filter === 'current'} onClick={() => setFilter('current')} />
            <FilterButton label="共享池" active={filter === 'shared'} onClick={() => setFilter('shared')} />
          </div>
          <p className="safe-note">来源按当前 Codex Home 优先；共享池只补缺，不反向覆盖归档状态。</p>
        </aside>

        <section className="session-table-card">
          <div className="session-table-head">
            <span>选择</span>
            <span>会话</span>
            <span>状态</span>
            <span>来源</span>
            <span>更新时间</span>
          </div>
          {sessions.length === 0 ? (
            <p className="empty-state">当前筛选下没有会话。</p>
          ) : (
            sessions.map((session) => (
              <SessionRow
                key={session.id}
                session={session}
                selected={selectedIds.has(session.id)}
                onToggle={() => toggleSession(session.id)}
              />
            ))
          )}
        </section>

        <aside className="detail-panel selected-session-panel">
          <div className="card-title-row">
            <span className="card-icon">🗂️</span>
            <div>
              <p className="eyebrow">所选会话</p>
              <h2>{numberFormat.format(selectedIds.size)} 个</h2>
            </div>
          </div>
          <dl className="compact-meta">
            <div>
              <dt>未归档</dt>
              <dd>{numberFormat.format(selectedUnarchived)}</dd>
            </div>
            <div>
              <dt>已归档</dt>
              <dd>{numberFormat.format(selectedArchived)}</dd>
            </div>
          </dl>
          <div className="detail-actions">
            <button onClick={restoreSelected} disabled={busy || selectedIds.size === 0}>
              恢复可见
            </button>
            <button className="danger" onClick={deleteSelected} disabled={busy || selectedIds.size === 0}>
              删除所选
            </button>
            <button onClick={clearSelected} disabled={busy || selectedIds.size === 0}>
              清空选择
            </button>
          </div>
          <p className="safe-note">
            恢复只写当前 Codex Home；删除会同时处理当前 Codex Home 与 shared-sessions，失败时保留备份并报错。
          </p>
        </aside>
      </section>
    </section>
  );
}

function SessionRow({
  session,
  selected,
  onToggle,
}: {
  session: ManagedSessionRecord;
  selected: boolean;
  onToggle: () => void;
}) {
  return (
    <label className={`session-row ${selected ? 'selected' : ''}`}>
      <span>
        <input type="checkbox" checked={selected} onChange={onToggle} aria-label={`选择 ${session.id}`} />
      </span>
      <span>
        <strong>{session.title || session.preview || session.id}</strong>
        <small>{session.id}</small>
      </span>
      <span className={`pill ${session.archived ? 'orange' : 'teal'}`}>
        {session.archived ? '已归档' : '未归档'}
      </span>
      <span>{sourceLabel(session.scope)}</span>
      <span>{formatTime(session.updatedAtMs ?? session.updatedAt)}</span>
    </label>
  );
}

function FilterButton({
  label,
  active,
  onClick,
}: {
  label: string;
  active: boolean;
  onClick: () => void;
}) {
  return (
    <button className={active ? 'active' : ''} onClick={onClick} role="tab" aria-selected={active}>
      {label}
    </button>
  );
}

function matchesFilter(session: ManagedSessionRecord, filter: SessionFilter) {
  switch (filter) {
    case 'visible':
      return !session.archived;
    case 'archived':
      return session.archived;
    case 'current':
      return session.scope === 'current' || session.scope === 'both';
    case 'shared':
      return session.scope === 'shared' || session.scope === 'both';
    default:
      return true;
  }
}

function sourceLabel(scope: ManagedSessionRecord['scope']) {
  switch (scope) {
    case 'current':
      return '本机';
    case 'shared':
      return '共享池';
    case 'both':
      return '两边都有';
    default:
      return '未知';
  }
}

function formatTime(value: number | null) {
  if (!value) return '未知';
  const millis = value > 10_000_000_000 ? value : value * 1000;
  return new Date(millis).toLocaleString('zh-CN', { hour12: false });
}
