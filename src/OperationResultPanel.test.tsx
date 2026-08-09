import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { OperationResultPanel } from './OperationResultPanel';

describe('OperationResultPanel diagnostics', () => {
  it('offers direct diagnostics for a rolled-back receipt with its real operation id', () => {
    render(<OperationResultPanel result={{
      label: '会话删除完成',
      operationId: 'delete-1',
      metrics: ['删除线程：0'],
      rolledBack: true,
    }} />);

    expect(screen.getByText('结果：已回滚')).toBeTruthy();
    expect(screen.getByRole('button', { name: '导出本次诊断' })).toBeTruthy();
    expect(screen.getByText('已自动脱敏，不含凭据和聊天内容')).toBeTruthy();
  });

  it('does not add a failure export action to an ordinary success receipt', () => {
    render(<OperationResultPanel result={{ label: '完整备份已创建', metrics: [] }} />);

    expect(screen.getByText('结果：成功')).toBeTruthy();
    expect(screen.queryByRole('button', { name: /导出.*诊断/ })).toBeNull();
  });
});
