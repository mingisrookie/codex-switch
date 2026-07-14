import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import { SkillsManagementPage } from './SkillsManagementPage';
import type { SkillMutationReceipt, SkillStatus } from './types';

const statuses: SkillStatus[] = [
  {
    id: 'image2', displayName: 'Image2', description: 'gpt-image-2 图片生成与编辑',
    installedPath: 'C:\\Codex\\skills\\newapi-image2-client', state: 'missing',
    bundledVersion: '2026.07.14', installedVersion: null, canInstall: true, canUpdate: false,
    baseUrl: 'https://api.lcming951.com/v1', credentialConfigured: false,
    restartRequired: false, message: '尚未安装',
  },
  {
    id: 'grokSearch', displayName: 'Grok 搜索', description: 'Web 与 X 实时搜索',
    installedPath: 'C:\\Codex\\skills\\grok-search', state: 'current',
    bundledVersion: '2026.07.14', installedVersion: '2026.07.14', canInstall: false, canUpdate: false,
    baseUrl: 'https://research.example.com', credentialConfigured: true,
    restartRequired: false, message: '已是最新版本',
  },
];

const receipt: SkillMutationReceipt = {
  operationId: 'skill-1', skillId: 'image2', action: 'install', installedVersion: '2026.07.14',
  backupDir: null, rolledBack: false, restartRequired: true, warnings: [],
};

describe('SkillsManagementPage', () => {
  it('loads lazily on first activation and keeps the loaded state', async () => {
    const listSkills = vi.fn().mockResolvedValue(statuses);
    const props = {
      active: false, busy: false, onBusyChange: vi.fn(), ensureCodexClosed: vi.fn(),
      listSkills, installSkill: vi.fn(), saveSkillConfig: vi.fn(),
    };
    const view = render(<SkillsManagementPage {...props} />);
    expect(listSkills).not.toHaveBeenCalled();

    view.rerender(<SkillsManagementPage {...props} active />);
    expect(await screen.findByRole('heading', { name: '技能安装与配置' })).toBeTruthy();
    await waitFor(() => expect(listSkills).toHaveBeenCalledTimes(1));
    expect(screen.getByText('Image2')).toBeTruthy();

    view.rerender(<SkillsManagementPage {...props} active={false} />);
    view.rerender(<SkillsManagementPage {...props} active />);
    expect(listSkills).toHaveBeenCalledTimes(1);
  });

  it('installs a missing skill after the Codex close preflight and shows the receipt', async () => {
    const ensureCodexClosed = vi.fn().mockResolvedValue(undefined);
    const installSkill = vi.fn().mockResolvedValue(receipt);
    render(<SkillsManagementPage
      active busy={false} onBusyChange={vi.fn()} ensureCodexClosed={ensureCodexClosed}
      listSkills={vi.fn().mockResolvedValue(statuses)} installSkill={installSkill}
      saveSkillConfig={vi.fn()}
    />);

    fireEvent.click(await screen.findByRole('button', { name: '安装 Image2' }));
    await waitFor(() => expect(ensureCodexClosed).toHaveBeenCalledWith('技能安装'));
    expect(installSkill).toHaveBeenCalledWith('image2', false);
    expect(await screen.findByText('技能安装完成')).toBeTruthy();
    expect(screen.getByText('重启 Codex 后生效')).toBeTruthy();
  });

  it('keeps the password only for a failed retry and clears it after success', async () => {
    const saveSkillConfig = vi.fn()
      .mockRejectedValueOnce(new Error('保存失败'))
      .mockResolvedValueOnce({ ...receipt, action: 'configure' });
    render(<SkillsManagementPage
      active busy={false} onBusyChange={vi.fn()} ensureCodexClosed={vi.fn()}
      listSkills={vi.fn().mockResolvedValue(statuses)} installSkill={vi.fn()}
      saveSkillConfig={saveSkillConfig}
    />);

    fireEvent.click(await screen.findByRole('button', { name: '配置 Grok 搜索' }));
    const key = screen.getByLabelText('API Key') as HTMLInputElement;
    expect(key.type).toBe('password');
    expect(key.value).toBe('');
    fireEvent.change(screen.getByLabelText('服务 URL'), { target: { value: 'https://research.example.com' } });
    fireEvent.change(key, { target: { value: 'sk-user-secret' } });
    fireEvent.click(screen.getByRole('button', { name: '保存技能配置' }));

    expect((await screen.findByRole('alert')).textContent).toContain('保存失败');
    expect((screen.getByLabelText('API Key') as HTMLInputElement).value).toBe('sk-user-secret');

    fireEvent.click(screen.getByRole('button', { name: '保存技能配置' }));
    await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
    expect(screen.queryByDisplayValue('sk-user-secret')).toBeNull();
  });
});
