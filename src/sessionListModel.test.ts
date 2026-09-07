import { describe, expect, it } from 'vitest';
import { createSessionIndex, filterSessionIndex, sessionTimestampMs, sortSessionIndex } from './sessionListModel';
import type { ManagedSessionRecord } from './types';

function session(id: string, overrides: Partial<ManagedSessionRecord> = {}): ManagedSessionRecord {
  return {
    id, title: id, preview: null, modelProvider: 'openai', updatedAt: null,
    updatedAtMs: null, archived: false, archivedAt: null, scope: 'current', current: null, shared: null,
    ...overrides,
  };
}

const ids = (entries: ReturnType<typeof createSessionIndex>) => entries.map(({ session: item }) => item.id);

describe('session list model', () => {
  it('normalizes legacy seconds before comparing with explicit milliseconds', () => {
    const index = createSessionIndex([
      session('older', { updatedAtMs: 1_700_000_000_000 }),
      session('newer', { updatedAt: 1_800_000_000 }),
    ]);
    expect(ids(sortSessionIndex(index, 'updated-desc'))).toEqual(['newer', 'older']);
    expect(ids(sortSessionIndex(index, 'updated-asc'))).toEqual(['older', 'newer']);
  });

  it('retains legacy millisecond values and gives the explicit field priority', () => {
    expect(sessionTimestampMs({ updatedAt: 1_700_000_000_000, updatedAtMs: null })).toBe(1_700_000_000_000);
    expect(sessionTimestampMs({ updatedAt: 1_700_000_000, updatedAtMs: 1000 })).toBe(1000);
    expect(sessionTimestampMs({ updatedAt: 123, updatedAtMs: 0 })).toBe(0);
  });

  it.each([null, NaN, Infinity, -Infinity, 8_640_000_000_000_001])('treats invalid/missing timestamp %s as unknown', (value) => {
    expect(sessionTimestampMs({ updatedAtMs: value, updatedAt: null })).toBeNull();
  });

  it.each(['updated-asc', 'updated-desc'] as const)('keeps unknown dates last for %s', (sort) => {
    const index = createSessionIndex([session('missing'), session('epoch', { updatedAtMs: 0 }), session('invalid', { updatedAtMs: NaN })]);
    expect(ids(sortSessionIndex(index, sort))).toEqual(['epoch', 'invalid', 'missing']);
  });

  it.each(['updated-asc', 'updated-desc', 'title-asc', 'title-desc'] as const)('breaks ties deterministically by ID for %s', (sort) => {
    const sessions = [session('c', { title: '相同' }), session('a', { title: '相同' }), session('b', { title: '相同' })];
    expect(ids(sortSessionIndex(createSessionIndex(sessions), sort))).toEqual(['a', 'b', 'c']);
    expect(ids(sortSessionIndex(createSessionIndex([...sessions].reverse()), sort))).toEqual(['a', 'b', 'c']);
  });

  it('sorts title fallbacks without mutating the input index or inventory', () => {
    const sessions = [session('z', { title: null, preview: 'Zulu' }), session('a', { title: 'Alpha' })];
    const index = createSessionIndex(sessions);
    expect(ids(sortSessionIndex(index, 'title-asc'))).toEqual(['a', 'z']);
    expect(ids(sortSessionIndex(index, 'title-desc'))).toEqual(['z', 'a']);
    expect(ids(index)).toEqual(['z', 'a']);
    expect(sessions.map((item) => item.id)).toEqual(['z', 'a']);
  });

  it('indexes all searchable fields once and supports literal case-insensitive queries', () => {
    const entry = session('thread-needle', {
      title: 'TITLE', preview: 'preview', modelProvider: 'openai_custom',
      current: { home: 'C:/fixture', sessionFile: 'C:/fixture/current.jsonl', rolloutPath: 'C:/fixture/a[b].jsonl', archived: false, archivedAt: null, updatedAt: null, updatedAtMs: null },
      shared: { home: 'D:/fixture', sessionFile: 'D:/fixture/shared.jsonl', rolloutPath: 'D:/fixture/shared-rollout.jsonl', archived: false, archivedAt: null, updatedAt: null, updatedAtMs: null },
    });
    const index = createSessionIndex([entry]);
    for (const query of [' THREAD-NEEDLE ', 'title', 'PREVIEW', 'OPENAI_CUSTOM', 'CURRENT.JSONL', 'a[b]', 'shared.jsonl', 'shared-rollout']) {
      expect(filterSessionIndex(index, 'all', query)).toEqual([entry]);
    }
    expect(filterSessionIndex(index, 'all', 'not found')).toEqual([]);
    expect(filterSessionIndex(index, 'all', '   ')).toEqual([entry]);
  });

  it('combines text search with the existing archive and location filters', () => {
    const index = createSessionIndex([
      session('a', { scope: 'current' }), session('b', { scope: 'shared', archived: true }),
      session('c', { scope: 'both' }), session('d', { scope: 'unknown' }),
    ]);
    const matching = (filter: Parameters<typeof filterSessionIndex>[1], query = '') => filterSessionIndex(index, filter, query).map((item) => item.id);
    expect(matching('current')).toEqual(['a', 'c']);
    expect(matching('shared')).toEqual(['b', 'c']);
    expect(matching('visible')).toEqual(['a', 'c', 'd']);
    expect(matching('archived')).toEqual(['b']);
    expect(matching('shared', 'c')).toEqual(['c']);
  });

  it('keeps empty inventories empty for every sort and filter', () => {
    const index = sortSessionIndex(createSessionIndex([]), 'updated-desc');
    expect(filterSessionIndex(index, 'all', '')).toEqual([]);
    expect(filterSessionIndex(index, 'current', 'needle')).toEqual([]);
  });
});
