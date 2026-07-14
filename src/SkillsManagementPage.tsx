import { useEffect, useRef, useState } from 'react';
import {
  installSkill as defaultInstallSkill,
  listSkills as defaultListSkills,
  saveSkillConfig as defaultSaveSkillConfig,
} from './api';
import { OperationResultPanel, type OperationView } from './OperationResultPanel';
import type {
  DomainState,
  SkillConfigInput,
  SkillId,
  SkillMutationReceipt,
  SkillState,
  SkillStatus,
} from './types';

type SkillsManagementPageProps = {
  active: boolean;
  busy: boolean;
  onBusyChange: (label: string | null) => void;
  ensureCodexClosed: (reason: string) => Promise<void>;
  listSkills?: () => Promise<SkillStatus[]>;
  installSkill?: (skillId: SkillId, confirmReplace: boolean) => Promise<SkillMutationReceipt>;
  saveSkillConfig?: (input: SkillConfigInput) => Promise<SkillMutationReceipt>;
};

export function SkillsManagementPage({
  active,
  busy,
  onBusyChange,
  ensureCodexClosed,
  listSkills = defaultListSkills,
  installSkill = defaultInstallSkill,
  saveSkillConfig = defaultSaveSkillConfig,
}: SkillsManagementPageProps) {
  const [state, setState] = useState<DomainState<SkillStatus[]>>({ status: 'loading' });
  const [error, setError] = useState<string | null>(null);
  const [receipt, setReceipt] = useState<OperationView | null>(null);
  const [configuring, setConfiguring] = useState<SkillStatus | null>(null);
  const [configError, setConfigError] = useState<string | null>(null);
  const loaded = useRef(false);
  const requestId = useRef(0);

  useEffect(() => {
    if (!active || loaded.current) return;
    loaded.current = true;
    void refreshSkills();
  }, [active]);

  async function refreshSkills() {
    const current = ++requestId.current;
    setState({ status: 'loading' });
    try {
      const skills = await listSkills();
      if (current === requestId.current) setState({ status: 'ready', data: skills });
      return true;
    } catch (reason) {
      if (current === requestId.current) setState({ status: 'error', error: errorMessage(reason) });
      return false;
    }
  }

  function refreshInBackground(successMessage: string) {
    void refreshSkills().then((ok) => {
      if (!ok) setError(`${successMessage}，但状态刷新失败`);
    });
  }

  async function handleInstall(skill: SkillStatus) {
    const replacing = ['drifted', 'unmanaged', 'invalid'].includes(skill.state);
    if (replacing && !window.confirm(`${skill.displayName} 已存在且不是当前受管版本。将先完整备份再覆盖，确认继续？`)) return;
    onBusyChange('技能安装');
    setError(null);
    setReceipt(null);
    try {
      await ensureCodexClosed('技能安装');
      const result = await installSkill(skill.id, replacing);
      setReceipt(receiptView(result));
      refreshInBackground('技能安装已成功');
    } catch (reason) {
      setError(errorMessage(reason));
      void refreshSkills().catch(() => undefined);
    } finally {
      onBusyChange(null);
    }
  }

  async function handleSaveConfig(input: SkillConfigInput) {
    onBusyChange('技能配置');
    setConfigError(null);
    setError(null);
    setReceipt(null);
    try {
      await ensureCodexClosed('技能配置');
      const result = await saveSkillConfig(input);
      setReceipt(receiptView(result));
      setConfiguring(null);
      refreshInBackground('技能配置已保存');
      return true;
    } catch (reason) {
      setConfigError(errorMessage(reason));
      return false;
    } finally {
      onBusyChange(null);
    }
  }

  return (
    <section className="skills-page" hidden={!active} aria-label="技能安装与配置">
      <section className="hero-card skills-hero">
        <div>
          <p className="eyebrow">固定来源 · 本机加密</p>
          <h1>技能安装与配置</h1>
          <p className="lede">把 Image2 与 Grok 搜索安装到当前 Codex Home。API Key 只使用 Windows DPAPI 加密保存。</p>
        </div>
        <button className="ghost-button inline" onClick={() => void refreshSkills()} disabled={busy}>刷新技能状态</button>
      </section>

      {error ? <p className="error-banner" role="alert">{error}</p> : null}
      {receipt ? <OperationResultPanel result={receipt} /> : null}

      {state.status === 'loading' ? <p className="domain-placeholder" role="status">正在扫描技能...</p> : null}
      {state.status === 'error' ? (
        <div className="domain-placeholder" role="alert">
          <p>技能状态读取失败：{state.error}</p>
          <button className="ghost-button inline" onClick={() => void refreshSkills()} disabled={busy}>重试</button>
        </div>
      ) : null}
      {state.status === 'ready' ? (
        <div className="skills-grid">
          {state.data.map((skill) => (
            <article className="skill-card" key={skill.id}>
              <div className="skill-card-heading">
                <div>
                  <p className="eyebrow">{skill.id === 'image2' ? 'gpt-image-2' : 'Web + X'}</p>
                  <h2>{skill.displayName}</h2>
                </div>
                <span className={`skill-state state-${skill.state}`}>{skillStateLabel(skill.state)}</span>
              </div>
              <p>{skill.description}</p>
              <dl className="skill-details">
                <div><dt>内置版本</dt><dd>{skill.bundledVersion}</dd></div>
                <div><dt>已装版本</dt><dd>{skill.installedVersion ?? '未安装'}</dd></div>
                <div><dt>服务 URL</dt><dd title={skill.baseUrl}>{skill.baseUrl || '尚未配置'}</dd></div>
                <div><dt>API Key</dt><dd>{skill.credentialConfigured ? '已加密配置' : '未配置'}</dd></div>
                <div><dt>安装路径</dt><dd title={skill.installedPath}>{skill.installedPath}</dd></div>
              </dl>
              <p className="safe-note">{skill.message}</p>
              <div className="skill-actions">
                {skill.canInstall || skill.canUpdate ? (
                  <button className="primary-button" disabled={busy} onClick={() => void handleInstall(skill)}>
                    {skillActionLabel(skill)} {skill.displayName}
                  </button>
                ) : null}
                {canConfigure(skill.state) ? (
                  <button className="ghost-button inline" disabled={busy} onClick={() => {
                    setConfigError(null);
                    setConfiguring(skill);
                  }}>配置 {skill.displayName}</button>
                ) : null}
              </div>
            </article>
          ))}
        </div>
      ) : null}

      {configuring ? (
        <SkillConfigDialog
          skill={configuring}
          busy={busy}
          submitError={configError}
          onCancel={() => { setConfigError(null); setConfiguring(null); }}
          onSave={handleSaveConfig}
        />
      ) : null}
    </section>
  );
}

function SkillConfigDialog({
  skill,
  busy,
  submitError,
  onCancel,
  onSave,
}: {
  skill: SkillStatus;
  busy: boolean;
  submitError: string | null;
  onCancel: () => void;
  onSave: (input: SkillConfigInput) => Promise<boolean>;
}) {
  const [baseUrl, setBaseUrl] = useState(skill.baseUrl);
  const [apiKey, setApiKey] = useState('');
  const [localError, setLocalError] = useState<string | null>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);

  useEffect(() => {
    const dialog = dialogRef.current;
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    if (dialog) {
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
    }
    return () => {
      if (dialog?.open && typeof dialog.close === 'function') dialog.close();
      setApiKey('');
      previousFocus?.focus();
    };
  }, []);

  async function submit(event: React.FormEvent) {
    event.preventDefault();
    const normalizedUrl = normalizeInputUrl(baseUrl);
    setLocalError(null);
    if (!normalizedUrl) {
      setLocalError('服务 URL 必须是有效的 http 或 https 地址，且不能包含凭据、查询参数或片段');
      return;
    }
    if (normalizedUrl.protocol === 'http:' && !isLoopbackHost(normalizedUrl.hostname)) {
      setLocalError('非本机服务 URL 必须使用 HTTPS');
      return;
    }
    if (!skill.credentialConfigured && !apiKey.trim()) {
      setLocalError('首次配置必须填写 API Key');
      return;
    }
    const saved = await onSave({ skillId: skill.id, baseUrl: normalizedUrl.toString(), apiKey: apiKey.trim() });
    if (saved) setApiKey('');
  }

  const visibleError = localError ?? submitError;
  return (
    <dialog
      ref={dialogRef}
      className="relay-dialog skill-config-dialog"
      aria-labelledby="skill-config-title"
      aria-describedby="skill-config-note skill-config-error"
      onCancel={(event) => { event.preventDefault(); if (!busy) onCancel(); }}
    >
      <div className="card-title-row">
        <span className="card-icon">🔐</span>
        <div><p className="eyebrow">Windows DPAPI</p><h2 id="skill-config-title">配置 {skill.displayName}</h2></div>
      </div>
      <form onSubmit={(event) => void submit(event)}>
        <label className="dialog-field">
          <span>服务 URL</span>
          <input aria-label="服务 URL" inputMode="url" value={baseUrl} onChange={(event) => setBaseUrl(event.target.value)} autoFocus />
        </label>
        <label className="dialog-field">
          <span>API Key</span>
          <input
            aria-label="API Key"
            type="password"
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
            autoComplete="new-password"
            placeholder={skill.credentialConfigured ? '留空则保留已加密保存的 Key' : '首次配置必填'}
          />
        </label>
        {visibleError ? <p className="form-error" id="skill-config-error" role="alert">{visibleError}</p> : <span id="skill-config-error" />}
        <p className="safe-note" id="skill-config-note">Key 不会回填或显示；保存成功或取消后立即清空本页输入。</p>
        <div className="dialog-actions">
          <button type="button" className="ghost-button inline" onClick={onCancel} disabled={busy}>取消</button>
          <button type="submit" className="primary-button" disabled={busy}>保存技能配置</button>
        </div>
      </form>
    </dialog>
  );
}

function receiptView(receipt: SkillMutationReceipt): OperationView {
  const labels = { install: '技能安装完成', update: '技能更新完成', configure: '技能配置已保存' };
  return {
    label: labels[receipt.action],
    operationId: receipt.operationId,
    metrics: [
      `技能：${receipt.skillId === 'image2' ? 'Image2' : 'Grok 搜索'}`,
      `版本：${receipt.installedVersion}`,
      ...(receipt.restartRequired ? ['重启 Codex 后生效'] : []),
    ],
    backupCount: receipt.backupDir ? 1 : 0,
    backupPaths: receipt.backupDir ? [receipt.backupDir] : [],
    rolledBack: receipt.rolledBack,
    warnings: receipt.warnings,
  };
}

function skillStateLabel(state: SkillState) {
  const labels: Record<SkillState, string> = {
    missing: '未安装', current: '已安装', updateAvailable: '可更新',
    drifted: '本地已修改', unmanaged: '非受管目录', invalid: '安装异常',
  };
  return labels[state];
}

function skillActionLabel(skill: SkillStatus) {
  if (skill.state === 'missing') return '安装';
  if (skill.state === 'updateAvailable') return '更新';
  return '覆盖安装';
}

function canConfigure(state: SkillState) {
  return state === 'current' || state === 'updateAvailable' || state === 'drifted';
}

function normalizeInputUrl(value: string) {
  const raw = value.trim();
  if (!raw) return null;
  try {
    const url = new URL(/^[a-z][a-z\d+.-]*:\/\//i.test(raw) ? raw : `https://${raw}`);
    if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password || url.search || url.hash) return null;
    return url;
  } catch {
    return null;
  }
}

function isLoopbackHost(hostname: string) {
  return hostname === 'localhost' || hostname === '127.0.0.1' || hostname === '[::1]' || hostname === '::1';
}

function errorMessage(reason: unknown) {
  return reason instanceof Error ? reason.message : String(reason);
}
