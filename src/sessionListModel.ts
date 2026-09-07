import type { ManagedSessionRecord } from './types';

export type SessionScopeFilter = 'all' | 'visible' | 'archived' | 'current' | 'shared';
export type SessionSort = 'updated-desc' | 'updated-asc' | 'title-asc' | 'title-desc';

export type SessionListEntry = {
  session: ManagedSessionRecord;
  searchText: string;
  title: string;
  updatedAtMs: number | null;
};

const titleCollator = new Intl.Collator('zh-CN');
const maxDateMs = 8_640_000_000_000_000;

export function sessionTimestampMs(session: Pick<ManagedSessionRecord, 'updatedAtMs' | 'updatedAt'>): number | null {
  // The explicit millisecond field must never pass through the legacy unit heuristic.
  const millis = session.updatedAtMs ?? (session.updatedAt === null
    ? null
    : Math.abs(session.updatedAt) > 10_000_000_000 ? session.updatedAt : session.updatedAt * 1000);
  return millis !== null && Number.isFinite(millis) && Math.abs(millis) <= maxDateMs ? millis : null;
}

export function createSessionIndex(sessions: readonly ManagedSessionRecord[]): SessionListEntry[] {
  return sessions.map((session) => ({
    session,
    title: session.title || session.preview || '',
    updatedAtMs: sessionTimestampMs(session),
    searchText: [
      session.id,
      session.title,
      session.preview,
      session.modelProvider,
      session.current?.sessionFile,
      session.current?.rolloutPath,
      session.shared?.sessionFile,
      session.shared?.rolloutPath,
    ].filter(Boolean).join('\n').toLocaleLowerCase('zh-CN'),
  }));
}

export function sortSessionIndex(index: readonly SessionListEntry[], sort: SessionSort): SessionListEntry[] {
  return [...index].sort((left, right) => {
    let order: number;
    if (sort === 'title-asc' || sort === 'title-desc') {
      order = titleCollator.compare(left.title, right.title) * (sort === 'title-desc' ? -1 : 1);
    } else {
      // Unknown dates remain at the end in either direction, rather than masquerading as 1970.
      if (left.updatedAtMs === null && right.updatedAtMs !== null) return 1;
      if (right.updatedAtMs === null && left.updatedAtMs !== null) return -1;
      order = ((left.updatedAtMs ?? 0) - (right.updatedAtMs ?? 0)) * (sort === 'updated-desc' ? -1 : 1);
    }
    return order || (left.session.id < right.session.id ? -1 : left.session.id > right.session.id ? 1 : 0);
  });
}

export function filterSessionIndex(
  index: readonly SessionListEntry[],
  filter: SessionScopeFilter,
  query: string,
): ManagedSessionRecord[] {
  const needle = query.trim().toLocaleLowerCase('zh-CN');
  return index.filter(({ session, searchText }) => {
    if (filter === 'visible' && session.archived) return false;
    if (filter === 'archived' && !session.archived) return false;
    if (filter === 'current' && session.scope !== 'current' && session.scope !== 'both') return false;
    if (filter === 'shared' && session.scope !== 'shared' && session.scope !== 'both') return false;
    return !needle || searchText.includes(needle);
  }).map(({ session }) => session);
}
