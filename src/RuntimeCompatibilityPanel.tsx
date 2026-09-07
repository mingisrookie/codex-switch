import { ShieldCheck } from 'lucide-react';
import type { DomainState, RuntimeCompatibilityReport } from './types';

export function RuntimeCompatibilityPanel({
  state,
}: {
  state: DomainState<RuntimeCompatibilityReport>;
}) {
  const report = state.status === 'ready' ? state.data : null;
  const clientVersions = report?.managedClients
    .map((client) => `${client.packageName ?? 'ChatGPT/Codex'} ${client.version ?? '版本未知'}`)
    .join(' · ');
  return (
    <section
      className={`compatibility-panel compatibility-${report?.status ?? state.status}`}
      aria-label="ChatGPT 和 Codex 兼容性"
    >
      <div className="card-title-row">
        <ShieldCheck className="section-icon" aria-hidden="true" />
        <div><p className="eyebrow">CLIENT COMPATIBILITY</p><h2>本地结构兼容性</h2></div>
      </div>
      {state.status === 'loading' ? <p>正在只读检查客户端包版本、SQLite 完整性与关键表结构…</p>
        : state.status === 'error' ? <p role="alert">{state.error}</p>
          : report ? <>
            <div className="compatibility-grid">
              <span><strong>{compatibilityStatusLabel(report.status)}</strong><small>总体状态</small></span>
              <span><strong>{capabilityLabel(report.routeConfig)}</strong><small>请求配置</small></span>
              <span><strong>{capabilityLabel(report.sessionView)}</strong><small>会话视图</small></span>
              <span><strong>{capabilityLabel(report.advancedStorage)}</strong><small>高级存储</small></span>
            </div>
            <p className="compatibility-detail">{clientVersions || '未发现受管 ChatGPT/Codex 包版本；数据库结构仍按只读能力检查判定。'}</p>
            {report.schemaFingerprint ? <p className="compatibility-detail">Schema 指纹：<code>{report.schemaFingerprint.slice(0, 16)}…</code></p> : null}
            {report.issues.length ? <ul className="compatibility-issues">{report.issues.map((issue) => <li key={issue.code}>{issue.message}</li>)}</ul> : null}
          </> : null}
    </section>
  );
}

export function compatibilityStatusLabel(status: RuntimeCompatibilityReport['status']) {
  const labels: Record<RuntimeCompatibilityReport['status'], string> = {
    supported: '已支持',
    warning: '有限支持',
    unknown: '未知结构',
    blocked: '已阻止',
  };
  return labels[status];
}

function capabilityLabel(status: RuntimeCompatibilityReport['routeConfig']) {
  if (status === 'supported') return '可用';
  if (status === 'unavailable') return '待生成';
  return '已阻止';
}
