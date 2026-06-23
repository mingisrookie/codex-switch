import { useEffect, useMemo, useState } from 'react';
import {
  closeCodexProcesses,
  importPlusRuntime,
  listCodexProcesses,
  loadDashboard as defaultLoadDashboard,
  switchRuntime,
  syncAllSessions,
  upsertRelayRuntime,
} from './api';
import type { DashboardData, RuntimeMetadata } from './types';

type AppProps = {
  loadDashboard?: () => Promise<DashboardData>;
};

const numberFormat = new Intl.NumberFormat('zh-CN');

function App({ loadDashboard = defaultLoadDashboard }: AppProps) {
  const [data, setData] = useState<DashboardData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    loadDashboard()
      .then((next) => {
        if (!cancelled) {
          setData(next);
        }
      })
      .catch((reason: unknown) => {
        if (!cancelled) {
          setError(reason instanceof Error ? reason.message : '扫描失败');
        }
      });
    return () => {
      cancelled = true;
    };
  }, [loadDashboard]);

  const plusRuntime = useMemo(
    () => data?.runtimes.find((runtime) => runtime.kind === 'plus') ?? null,
    [data],
  );
  const relayRuntime = useMemo(
    () => data?.runtimes.find((runtime) => runtime.kind === 'relay') ?? null,
    [data],
  );
  const runtimeCount = data ? data.runtimes.length : 0;
  const authMode = data?.codexHome.authSummary?.authMode ?? '未检测';
  const threadCount = data ? numberFormat.format(data.sessions.threadCount) : '扫描中';
  const jsonlCount = data ? numberFormat.format(data.sessions.sessionJsonlCount) : '扫描中';

  async function refresh() {
    const next = await loadDashboard();
    setData(next);
  }

  async function runAction(label: string, action: () => Promise<unknown>) {
    setBusy(label);
    setError(null);
    try {
      await action();
      await refresh();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(null);
    }
  }

  async function ensureCodexClosed(reason: string) {
    const processes = await listCodexProcesses();
    if (processes.length === 0) return;

    const ok = window.confirm(`检测到 ${processes.length} 个 Codex 进程。${reason}需要关闭后继续，是否关闭？`);
    if (!ok) {
      throw new Error('用户取消操作');
    }
    await closeCodexProcesses();
  }

  async function handleImportPlus() {
    await runAction('保存 Codex 账号态', () => importPlusRuntime());
  }

  async function handleConfigureRelay() {
    const baseUrl = window.prompt('中转站 Base URL', relayRuntime?.baseUrl ?? 'https://your-relay.example.com/v1');
    if (!baseUrl) return;
    const model = window.prompt('模型', relayRuntime?.model ?? plusRuntime?.model ?? 'gpt-5.5') || 'gpt-5.5';
    const apiKey = window.prompt('API Key（只加密保存，不展示）', '');
    if (!apiKey?.trim()) {
      setError('API Key 不能为空');
      return;
    }

    await runAction('配置中转站', () =>
      upsertRelayRuntime({
        baseUrl: baseUrl.trim(),
        model: model.trim() || 'gpt-5.5',
        apiKey: apiKey.trim(),
      }),
    );
  }

  async function handleSwitch(runtimeId: RuntimeMetadata['id'], label: string) {
    await runAction(label, async () => {
      await ensureCodexClosed('运行态切换');
      await switchRuntime(runtimeId);
    });
  }

  async function handleSyncSessions() {
    await runAction('同步会话', async () => {
      await syncAllSessions();
    });
  }

  return (
    <main className="app-shell">
      <header className="topbar">
        <div className="brand-dot">CS</div>
        <strong>CODEX SWITCH</strong>
        <span className="topbar-mode">运行态</span>
        <button className="ghost-button" onClick={refresh} disabled={busy !== null}>刷新</button>
      </header>

      <section className="hero-card account-hero">
        <div>
          <p className="eyebrow">Codex 账号态 ↔ API 中转站</p>
          <h1>Codex 运行态切换</h1>
          <p className="lede">
            保存当前 Codex 账号态和一个 API 中转站态。运行态切换需要关闭确认和自动备份；
            会话同步支持热同步，Codex 运行时也可以把 SQLite 与 JSONL 合并到共享会话池。
          </p>
          <div className="hero-meta" aria-label="当前扫描摘要">
            <span>auth：{authMode}</span>
            <span>运行态：{runtimeCount}</span>
            <span>会话：{threadCount}</span>
            <span>JSONL：{jsonlCount}</span>
          </div>
        </div>
        <div className="hero-actions">
          <button className="primary-button" onClick={handleImportPlus} disabled={busy !== null}>
            保存当前账号态
          </button>
          <button className="warm-button" onClick={handleConfigureRelay} disabled={busy !== null}>
            配置 API 中转站
          </button>
        </div>
      </section>

      {error ? <p className="error-banner">{error}</p> : null}
      {busy ? <p className="busy-banner">{busy}处理中...</p> : null}

      <section className="runtime-grid" aria-label="运行态与会话">
        <RuntimeCard
          icon="👤"
          title="Codex 账号态"
          eyebrow="账号登录态"
          description="适用于不同等级的 Codex 账号。保存当前 auth.json 与完整 config.toml，用于切回账号登录。"
          runtime={plusRuntime}
          fallbackStatus="尚未保存"
          statusLabel="账号态已保存"
          baseUrlFallback="本机 Codex 登录态"
          credentialLabel="auth.json 已加密保存"
          primaryAction="保存当前账号态"
          onPrimary={handleImportPlus}
          switchAction="切换到 Codex 账号"
          onSwitch={() => handleSwitch('plus', '切换 Codex 账号')}
          switchDisabled={!plusRuntime || busy !== null}
          busy={busy !== null}
        />

        <RuntimeCard
          icon="🔑"
          title="API 中转站态"
          eyebrow="API 调用态"
          description="只维护一个中转站：URL、模型和 API Key。Key 加密保存，切换时写入 auth.json 供 Codex 使用。"
          runtime={relayRuntime}
          fallbackStatus="尚未配置"
          statusLabel="中转站已配置"
          baseUrlFallback="尚未配置"
          credentialLabel="Key 已加密保存"
          primaryAction="配置中转站"
          onPrimary={handleConfigureRelay}
          switchAction="切换到中转站"
          onSwitch={() => handleSwitch('relay', '切换中转站')}
          switchDisabled={!relayRuntime || busy !== null}
          busy={busy !== null}
        />

        <aside className="detail-panel session-panel" aria-label="会话同步">
          <div className="card-title-row">
            <span className="card-icon">🔄</span>
            <div>
              <p className="eyebrow">运行中可用</p>
              <h2>会话热同步</h2>
            </div>
            <b className="status-pill">热同步</b>
          </div>
          <p className="hint no-indent">
            只合并 SQLite 与 sessions JSONL，不替换登录态文件；Codex 正在运行时也可以执行。
          </p>
          <div className="sync-stats">
            <strong>{threadCount}<span>threads</span></strong>
            <strong>{jsonlCount}<span>JSONL</span></strong>
          </div>
          <div className="usage-card">
            <Progress label="state_5.sqlite threads" value={data ? Math.min(100, data.sessions.threadCount % 100) : 0} />
            <Progress label="sessions JSONL" value={data ? Math.min(100, data.sessions.sessionJsonlCount % 100) : 0} />
            <button className="primary-button full" onClick={handleSyncSessions} disabled={busy !== null}>
              立即同步
            </button>
          </div>
          <p className="safe-note">敏感 token / API Key 不展示；运行态切换前确认关闭，单独会话同步支持热同步。</p>
        </aside>

        <aside className="detail-panel safety-panel" aria-label="安全检查">
          <div className="card-title-row">
            <span className="card-icon">🛡️</span>
            <div>
              <p className="eyebrow">安全检查</p>
              <h2>切换保护</h2>
            </div>
          </div>
          <SafetyLine ok label="真实 Token 不展示" />
          <SafetyLine ok label="运行态切换前自动备份" />
          <SafetyLine ok label="切换时确认关闭 Codex" />
          <SafetyLine ok label="会话同步支持热同步" />
        </aside>
      </section>
    </main>
  );
}

function RuntimeCard({
  icon,
  title,
  eyebrow,
  description,
  runtime,
  fallbackStatus,
  statusLabel,
  baseUrlFallback,
  credentialLabel,
  primaryAction,
  onPrimary,
  switchAction,
  onSwitch,
  switchDisabled,
  busy,
}: {
  icon: string;
  title: string;
  eyebrow: string;
  description: string;
  runtime: RuntimeMetadata | null;
  fallbackStatus: string;
  statusLabel: string;
  baseUrlFallback: string;
  credentialLabel: string;
  primaryAction: string;
  onPrimary: () => void;
  switchAction: string;
  onSwitch: () => void;
  switchDisabled: boolean;
  busy: boolean;
}) {
  return (
    <article className={`runtime-card ${runtime?.kind ?? 'empty'}`}>
      <div className="card-title-row">
        <span className="card-icon">{icon}</span>
        <div>
          <p className="eyebrow">{eyebrow}</p>
          <h2>{title}</h2>
        </div>
        <b className="status-pill">{runtime ? '可用' : '缺失'}</b>
      </div>
      <p className="hint no-indent">{description}</p>
      <dl className="meta-list">
        <div>
          <dt>状态</dt>
          <dd>{runtime ? statusLabel : fallbackStatus}</dd>
        </div>
        <div>
          <dt>Base URL</dt>
          <dd>{runtime?.baseUrl ?? baseUrlFallback}</dd>
        </div>
        <div>
          <dt>模型</dt>
          <dd>{runtime?.model ?? '跟随当前 Codex 配置'}</dd>
        </div>
        <div>
          <dt>凭据</dt>
          <dd>{runtime ? credentialLabel : '尚未保存'}</dd>
        </div>
      </dl>
      <div className="runtime-actions">
        <button className="ghost-button inline" onClick={onPrimary} disabled={busy}>{primaryAction}</button>
        <button className="switch-button" onClick={onSwitch} disabled={switchDisabled}>{switchAction}</button>
      </div>
    </article>
  );
}

function SafetyLine({ ok, label }: { ok: boolean; label: string }) {
  return (
    <div className="safety-line">
      <span>{ok ? '✓' : '!'}</span>
      <strong>{label}</strong>
    </div>
  );
}

function Progress({ label, value }: { label: string; value: number }) {
  return (
    <div className="progress-line">
      <span>{label}</span>
      <b>{value}%</b>
      <div>
        <i style={{ width: `${value}%` }} />
      </div>
    </div>
  );
}

export default App;
