import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';

const apiMocks = vi.hoisted(() => ({
  clearDiagnosticLogs: vi.fn(),
  exportDiagnostics: vi.fn(),
  getDiagnosticStatus: vi.fn(),
  openDiagnosticExport: vi.fn(),
  openDiagnosticLogDirectory: vi.fn(),
  retryDiagnosticExport: vi.fn(),
}));

vi.mock('./api', async () => {
  const actual = await vi.importActual<typeof import('./api')>('./api');
  return { ...actual, ...apiMocks };
});

import { DiagnosticExportAction, DiagnosticPanel } from './DiagnosticPanel';

const status = {
  available: true,
  eventCount: 12,
  totalBytes: 4096,
  retentionDays: 14,
  maxBytes: 10 * 1024 * 1024,
  oldestEventAtMs: 100,
  newestEventAtMs: 200,
  warnings: [],
};

describe('DiagnosticPanel', () => {
  beforeEach(() => {
    for (const mock of Object.values(apiMocks)) mock.mockReset();
    apiMocks.getDiagnosticStatus.mockResolvedValue(status);
    apiMocks.clearDiagnosticLogs.mockResolvedValue(undefined);
    apiMocks.openDiagnosticExport.mockResolvedValue(undefined);
    apiMocks.openDiagnosticLogDirectory.mockResolvedValue(undefined);
  });

  it('opens as a labelled page region, focuses its heading, and exposes no raw event viewer', async () => {
    const previous = document.createElement('button');
    document.body.append(previous);
    previous.focus();
    const view = render(<DiagnosticPanel onClose={vi.fn()} />);

    const panel = screen.getByRole('region', { name: '诊断与支持' });
    const heading = screen.getByRole('heading', { name: '诊断与支持' });
    await waitFor(() => expect(document.activeElement).toBe(heading));
    expect(panel.textContent).toContain('已自动脱敏，不含凭据和聊天内容');
    expect(await screen.findByText('12')).toBeTruthy();
    expect(screen.queryByText(/diagnostics\.jsonl|原始 JSON|事件列表/)).toBeNull();

    view.unmount();
    await waitFor(() => expect(document.activeElement).toBe(previous));
    previous.remove();
  });

  it('exports one package and opens only the backend-issued export id', async () => {
    apiMocks.exportDiagnostics.mockResolvedValue({
      exportId: 'export-1',
      path: 'C:\\Users\\alice\\Downloads\\ChatGPT-Switch-Diagnostics.zip',
      filename: 'ChatGPT-Switch-Diagnostics.zip',
      bytes: 2048,
      sha256: 'a'.repeat(64),
      eventCount: 7,
      warnings: [],
    });
    render(<DiagnosticPanel onClose={vi.fn()} />);

    fireEvent.click(screen.getByRole('button', { name: '导出最近诊断' }));
    expect(await screen.findByText('诊断包已保存')).toBeTruthy();
    expect(apiMocks.exportDiagnostics).toHaveBeenCalledWith(undefined);
    expect(screen.getByText(/ChatGPT-Switch-Diagnostics\.zip/)).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: '打开所在位置' }));
    await waitFor(() => expect(apiMocks.openDiagnosticExport).toHaveBeenCalledWith('export-1'));
  });

  it('uses inline clear confirmation and restores the clear trigger on cancel', async () => {
    const confirm = vi.spyOn(window, 'confirm');
    render(<DiagnosticPanel onClose={vi.fn()} />);
    const trigger = screen.getByRole('button', { name: '清除诊断日志' });

    fireEvent.click(trigger);
    const heading = screen.getByRole('heading', { name: '清除诊断日志？' });
    await waitFor(() => expect(document.activeElement).toBe(heading));
    expect(screen.queryByRole('dialog')).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: '取消' }));
    await waitFor(() => expect(document.activeElement).toBe(trigger));
    expect(confirm).not.toHaveBeenCalled();
  });

  it('uses a real operation id for scoped export and otherwise falls back to recent diagnostics', async () => {
    const retryId = 'diagnostic-export-context-aabbccddeeff00112233445566778899';
    apiMocks.exportDiagnostics.mockRejectedValue({
      kind: 'destination',
      message: '下载目录不可用',
      retryId,
    });
    apiMocks.retryDiagnosticExport.mockResolvedValue({
      exportId: 'export-fallback-1',
      path: 'C:\\Users\\alice\\AppData\\Local\\ChatGPT-Switch\\diagnostics\\diagnostics.zip',
      filename: 'diagnostics.zip',
      bytes: 1024,
      sha256: 'b'.repeat(64),
      eventCount: 3,
      warnings: [],
    });
    const view = render(<DiagnosticExportAction operationId="sync-1" />);

    fireEvent.click(screen.getByRole('button', { name: '导出本次诊断' }));
    expect((await screen.findByRole('alert')).textContent).toContain('下载目录导出失败：下载目录不可用');
    expect(apiMocks.exportDiagnostics).toHaveBeenCalledWith('sync-1');
    expect(apiMocks.retryDiagnosticExport).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: '重试下载目录' })).toBeTruthy();

    const fallback = screen.getByRole('button', { name: '改存应用诊断目录' });
    fireEvent.click(fallback);
    expect(await screen.findByText('诊断包已保存')).toBeTruthy();
    expect(apiMocks.retryDiagnosticExport).toHaveBeenCalledWith(retryId, 'diagnosticDirectory');
    await waitFor(() => expect(document.activeElement).toBe(
      screen.getByRole('button', { name: '导出本次诊断' }),
    ));
    fireEvent.click(screen.getByRole('button', { name: '打开所在位置' }));
    await waitFor(() => expect(apiMocks.openDiagnosticExport).toHaveBeenCalledWith('export-fallback-1'));

    view.rerender(<DiagnosticExportAction />);
    expect(screen.getByRole('button', { name: '导出最近诊断' })).toBeTruthy();
  });

  it('retries the Downloads directory explicitly and blocks duplicate clicks while exporting', async () => {
    const retryId = 'diagnostic-export-context-00112233445566778899aabbccddeeff';
    let resolveExport: ((receipt: object) => void) | undefined;
    apiMocks.exportDiagnostics.mockRejectedValueOnce({
      kind: 'destination',
      message: 'Downloads 被占用',
      retryId,
    });
    apiMocks.retryDiagnosticExport.mockImplementationOnce(
      () => new Promise((resolve) => { resolveExport = resolve; }),
    );
    render(<DiagnosticExportAction operationId="switch-1" />);

    fireEvent.click(screen.getByRole('button', { name: '导出本次诊断' }));
    const retry = await screen.findByRole('button', { name: '重试下载目录' });
    fireEvent.click(retry);
    fireEvent.click(retry);
    expect(apiMocks.exportDiagnostics).toHaveBeenCalledTimes(1);
    expect(apiMocks.retryDiagnosticExport).toHaveBeenCalledTimes(1);
    expect(apiMocks.retryDiagnosticExport).toHaveBeenCalledWith(retryId, 'downloads');

    resolveExport?.({
      exportId: 'export-retry-1',
      path: 'C:\\Users\\alice\\Downloads\\diagnostics.zip',
      filename: 'diagnostics.zip',
      bytes: 512,
      sha256: 'c'.repeat(64),
      eventCount: 2,
      warnings: [],
    });
    expect(await screen.findByText('诊断包已保存')).toBeTruthy();
  });

  it('shows preparation failures as a general export error without an invalid fallback', async () => {
    apiMocks.exportDiagnostics.mockRejectedValue({
      kind: 'preparation',
      message: 'diagnostic JSONL contains internal corruption',
    });
    render(<DiagnosticExportAction operationId="switch-prepare" />);

    fireEvent.click(screen.getByRole('button', { name: '导出本次诊断' }));

    expect((await screen.findByRole('alert')).textContent).toContain(
      '诊断导出失败：diagnostic JSONL contains internal corruption',
    );
    expect(screen.queryByRole('button', { name: '改存应用诊断目录' })).toBeNull();
    expect(screen.getByRole('button', { name: '导出本次诊断' })).toBeTruthy();
  });

  it('keeps the same retry context across fallback failure and a Downloads retry', async () => {
    const retryId = 'diagnostic-export-context-ffeeddccbbaa99887766554433221100';
    apiMocks.exportDiagnostics.mockRejectedValueOnce({
      kind: 'destination',
      message: 'Downloads unavailable',
      retryId,
    });
    apiMocks.retryDiagnosticExport
      .mockRejectedValueOnce({
        kind: 'destination',
        message: 'fallback unavailable',
        retryId,
      })
      .mockResolvedValueOnce({
        exportId: 'export-context-retry',
        path: 'C:\\isolated\\diagnostics.zip',
        filename: 'diagnostics.zip',
        bytes: 256,
        sha256: 'd'.repeat(64),
        eventCount: 4,
        selection: {
          mode: 'operation',
          operationId: 'switch-context',
          fromTimestampMs: 1,
          throughTimestampMs: 2,
        },
        warnings: [],
      });
    render(<DiagnosticExportAction operationId="switch-context" />);

    fireEvent.click(screen.getByRole('button', { name: '导出本次诊断' }));
    fireEvent.click(await screen.findByRole('button', { name: '改存应用诊断目录' }));
    expect((await screen.findByRole('alert')).textContent).toContain('fallback unavailable');
    fireEvent.click(screen.getByRole('button', { name: '重试下载目录' }));

    expect(await screen.findByText('诊断包已保存')).toBeTruthy();
    expect(apiMocks.retryDiagnosticExport.mock.calls).toEqual([
      [retryId, 'diagnosticDirectory'],
      [retryId, 'downloads'],
    ]);
  });

  it('blocks export while another diagnostic panel action is still running', async () => {
    let resolveOpen: (() => void) | undefined;
    apiMocks.openDiagnosticLogDirectory.mockImplementation(
      () => new Promise<void>((resolve) => { resolveOpen = resolve; }),
    );
    render(<DiagnosticPanel onClose={vi.fn()} />);
    await screen.findByText('12');

    fireEvent.click(screen.getByRole('button', { name: '打开日志目录' }));
    const exportButton = screen.getByRole('button', { name: '导出最近诊断' });
    expect(exportButton).toHaveProperty('disabled', true);
    fireEvent.click(exportButton);
    expect(apiMocks.exportDiagnostics).not.toHaveBeenCalled();

    resolveOpen?.();
    await waitFor(() => expect(exportButton).toHaveProperty('disabled', false));
  });
});
