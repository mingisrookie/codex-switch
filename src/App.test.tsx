import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DashboardData } from './types';

const apiMocks = vi.hoisted(() => ({
  importPlusRuntime: vi.fn(),
  upsertRelayRuntime: vi.fn(),
  listCodexProcesses: vi.fn(),
  closeCodexProcesses: vi.fn(),
  switchRuntime: vi.fn(),
  syncAllSessions: vi.fn(),
  deleteManagedSessions: vi.fn(),
  restoreSessionsVisible: vi.fn(),
}));

vi.mock('./api', () => ({
  loadDashboard: vi.fn(),
  importPlusRuntime: apiMocks.importPlusRuntime,
  upsertRelayRuntime: apiMocks.upsertRelayRuntime,
  listCodexProcesses: apiMocks.listCodexProcesses,
  closeCodexProcesses: apiMocks.closeCodexProcesses,
  switchRuntime: apiMocks.switchRuntime,
  syncAllSessions: apiMocks.syncAllSessions,
  deleteManagedSessions: apiMocks.deleteManagedSessions,
  restoreSessionsVisible: apiMocks.restoreSessionsVisible,
}));

import App from './App';

describe('App runtime switch UI', () => {
  beforeEach(() => {
    for (const mock of Object.values(apiMocks)) {
      mock.mockReset();
      mock.mockResolvedValue(undefined);
    }
    apiMocks.listCodexProcesses.mockResolvedValue([]);
    vi.restoreAllMocks();
  });

  function dashboardData(): DashboardData {
    return {
      codexHome: {
        root: 'C:\\Users\\alice\\.codex',
        sqliteHome: 'C:\\Users\\alice\\.codex',
        authJson: { path: 'auth.json', exists: true, bytes: 4525 },
        configToml: { path: 'config.toml', exists: true, bytes: 6585 },
        stateDb: { path: 'state_5.sqlite', exists: true, bytes: 12496896 },
        logsDb: { path: 'logs_2.sqlite', exists: true, bytes: 681955328 },
        codexDevDb: { path: 'sqlite/codex-dev.db', exists: true, bytes: 98304 },
        sessionsDir: { path: 'sessions', exists: true, bytes: null },
        sessionJsonlCount: 200,
        authSummary: {
          authMode: 'chatgpt',
          topLevelKeys: ['auth_mode', 'tokens'],
          hasTokensObject: true,
        },
      },
      sessions: {
        home: 'C:\\Users\\alice\\.codex',
        threadCount: 429,
        sessionJsonlCount: 200,
        threads: [],
        sessionFiles: [],
      },
      managedSessions: {
        currentHome: 'C:\\Users\\alice\\.codex',
        sharedHome: 'C:\\Users\\alice\\AppData\\Roaming\\codex-switch\\shared-sessions',
        totalCount: 2,
        archivedCount: 1,
        sessions: [
          {
            id: 'thread-visible',
            title: '当前会话',
            preview: null,
            modelProvider: 'openai',
            updatedAt: 1,
            updatedAtMs: 1000,
            archived: false,
            archivedAt: null,
            scope: 'both',
            current: null,
            shared: null,
          },
          {
            id: 'thread-archived',
            title: '归档会话',
            preview: null,
            modelProvider: 'openai',
            updatedAt: 2,
            updatedAtMs: 2000,
            archived: true,
            archivedAt: 2000,
            scope: 'shared',
            current: null,
            shared: null,
          },
        ],
      },
      runtimes: [
        {
          id: 'plus',
          name: 'Codex 账号',
          kind: 'plus',
          baseUrl: null,
          model: 'gpt-5.5',
          createdAtMs: 1,
          lastUsedAtMs: null,
        },
        {
          id: 'relay',
          name: 'API 中转站',
          kind: 'relay',
          baseUrl: 'https://relay.example.com/v1',
          model: 'gpt-5.5',
          createdAtMs: 2,
          lastUsedAtMs: null,
        },
      ],
    };
  }

  it('renders two runtime cards and session inventory without profile noise', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    expect(screen.getByText('CODEX SWITCH')).toBeTruthy();
    expect(screen.getByRole('heading', { name: 'Codex 运行态切换' })).toBeTruthy();
    expect(screen.getByRole('button', { name: '会话管理' })).toBeTruthy();
    expect(screen.queryByRole('button', { name: '设置' })).toBeNull();
    expect(await screen.findByText('Codex 账号态')).toBeTruthy();
    expect(screen.getByText('API 中转站态')).toBeTruthy();
    expect(screen.getByText('https://relay.example.com/v1')).toBeTruthy();
    expect(screen.getByText('429')).toBeTruthy();
    expect(screen.getByText('200')).toBeTruthy();
    expect(screen.queryByText('Plus 账号态')).toBeNull();
    expect(screen.queryByText('账号列表')).toBeNull();
  });

  it('configures the single relay runtime without rendering the key', async () => {
    vi.spyOn(window, 'prompt')
      .mockReturnValueOnce('www.example-relay.com')
      .mockReturnValueOnce('gpt-5.5')
      .mockReturnValueOnce('sk-fake-secret');

    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '配置中转站' }));

    await waitFor(() => {
      expect(apiMocks.upsertRelayRuntime).toHaveBeenCalledWith({
        baseUrl: 'www.example-relay.com',
        model: 'gpt-5.5',
        apiKey: 'sk-fake-secret',
      });
    });
    expect(screen.queryByText('sk-fake-secret')).toBeNull();
  });

  it('switches runtime only after close confirmation when Codex is running', async () => {
    apiMocks.listCodexProcesses.mockResolvedValue([{ imageName: 'codex.exe', pid: 1234 }]);
    vi.spyOn(window, 'confirm').mockReturnValue(true);

    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到中转站' }));

    await waitFor(() => {
      expect(apiMocks.listCodexProcesses).toHaveBeenCalled();
      expect(apiMocks.closeCodexProcesses).toHaveBeenCalled();
      expect(apiMocks.switchRuntime).toHaveBeenCalledWith('relay');
    });
  });

  it('does not switch runtime when user cancels closing Codex', async () => {
    apiMocks.listCodexProcesses.mockResolvedValue([{ imageName: 'codex.exe', pid: 1234 }]);
    vi.spyOn(window, 'confirm').mockReturnValue(false);

    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '切换到中转站' }));

    await waitFor(() => {
      expect(apiMocks.listCodexProcesses).toHaveBeenCalled();
      expect(apiMocks.switchRuntime).not.toHaveBeenCalled();
    });
  });

  it('hot-syncs sessions without closing Codex', async () => {
    apiMocks.listCodexProcesses.mockResolvedValue([{ imageName: 'codex.exe', pid: 1234 }]);

    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '立即同步' }));

    await waitFor(() => {
      expect(apiMocks.syncAllSessions).toHaveBeenCalled();
      expect(apiMocks.closeCodexProcesses).not.toHaveBeenCalled();
      expect(apiMocks.listCodexProcesses).not.toHaveBeenCalled();
    });
  });

  it('switches to session management without rendering exclude-sync action', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话管理' }));

    expect(screen.getByRole('heading', { name: '会话管理' })).toBeTruthy();
    expect(screen.getByText('当前会话')).toBeTruthy();
    expect(screen.getByText('归档会话')).toBeTruthy();
    expect(screen.getByText('两边都有')).toBeTruthy();
    expect(screen.getAllByText('共享池').length).toBeGreaterThan(0);
    expect(screen.queryByRole('button', { name: '排除同步' })).toBeNull();
  });

  it('requires confirmation before deleting unarchived sessions', async () => {
    vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话管理' }));
    fireEvent.click(screen.getByLabelText('选择 thread-visible'));
    fireEvent.click(screen.getByRole('button', { name: '删除所选' }));

    await waitFor(() => {
      expect(window.confirm).toHaveBeenCalled();
      expect(apiMocks.deleteManagedSessions).toHaveBeenCalledWith(['thread-visible'], true);
    });
  });

  it('deletes archived sessions without extra confirmation and restores visibility', async () => {
    const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话管理' }));
    fireEvent.click(screen.getByLabelText('选择 thread-archived'));
    fireEvent.click(screen.getByRole('button', { name: '删除所选' }));

    await waitFor(() => {
      expect(confirm).not.toHaveBeenCalled();
      expect(apiMocks.deleteManagedSessions).toHaveBeenCalledWith(['thread-archived'], false);
    });

    fireEvent.click(screen.getByRole('button', { name: '恢复可见' }));
    await waitFor(() => {
      expect(apiMocks.restoreSessionsVisible).toHaveBeenCalledWith(['thread-archived']);
    });
  });

  it('supports select-all and invert controls in session management', async () => {
    render(<App loadDashboard={() => Promise.resolve(dashboardData())} />);

    fireEvent.click(await screen.findByRole('button', { name: '会话管理' }));

    const bulkSelect = screen.getByLabelText('选择操作');
    expect(bulkSelect).toBeTruthy();
    expect(screen.queryByText('thread-visible')).toBeNull();
    expect(screen.queryByText('thread-archived')).toBeNull();

    fireEvent.change(bulkSelect, { target: { value: 'select-visible' } });
    expect((screen.getByLabelText('选择 thread-visible') as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText('选择 thread-archived') as HTMLInputElement).checked).toBe(true);

    fireEvent.change(bulkSelect, { target: { value: 'clear' } });
    expect((screen.getByLabelText('选择 thread-visible') as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText('选择 thread-archived') as HTMLInputElement).checked).toBe(false);

    fireEvent.click(screen.getByLabelText('全选当前列表'));

    expect((screen.getByLabelText('选择 thread-visible') as HTMLInputElement).checked).toBe(true);
    expect((screen.getByLabelText('选择 thread-archived') as HTMLInputElement).checked).toBe(true);

    fireEvent.click(screen.getByRole('button', { name: '反选' }));

    expect((screen.getByLabelText('选择 thread-visible') as HTMLInputElement).checked).toBe(false);
    expect((screen.getByLabelText('选择 thread-archived') as HTMLInputElement).checked).toBe(false);
  });
});
