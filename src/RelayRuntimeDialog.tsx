import { useEffect, useRef, useState } from 'react';
import { KeyRound, Save, X } from 'lucide-react';
import { DiagnosticExportAction } from './DiagnosticPanel';
import type { CommandErrorCode, CommandRecoverability } from './api';
import type { RelayRuntimeInput, RelayTransport, RuntimeMetadata } from './types';

type RelaySubmitFailure = {
  message: string;
  operationId?: string;
  code?: CommandErrorCode;
  recoverability?: CommandRecoverability;
};

type RelayRuntimeDialogProps = {
  runtime: RuntimeMetadata | null;
  fallbackModel: string;
  busy: boolean;
  submitError: RelaySubmitFailure | null;
  onCancel: () => void;
  onSave: (input: RelayRuntimeInput) => void | Promise<unknown>;
};

export function RelayRuntimeDialog({ runtime, fallbackModel, busy, submitError, onCancel, onSave }: RelayRuntimeDialogProps) {
  const [baseUrl, setBaseUrl] = useState(runtime?.baseUrl ?? '');
  const [model, setModel] = useState(runtime?.model ?? fallbackModel);
  const [transport, setTransport] = useState<RelayTransport>(runtime?.relayTransport ?? 'http');
  const [apiKey, setApiKey] = useState('');
  const [error, setError] = useState<string | null>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    headingRef.current?.scrollIntoView?.({ block: 'nearest' });
    headingRef.current?.focus();
    return () => {
      window.requestAnimationFrame(() => previousFocus?.focus());
    };
  }, []);

  function cancel() {
    setApiKey('');
    onCancel();
  }

  function submit(event: React.FormEvent) {
    event.preventDefault();
    const rawUrl = baseUrl.trim();
    const normalizedUrl = /^[a-z][a-z\d+.-]*:\/\//i.test(rawUrl) ? rawUrl : `https://${rawUrl}`;
    const normalizedModel = model.trim();
    const normalizedKey = apiKey.trim();
    setError(null);
    if (!rawUrl || !normalizedModel) {
      setError('Base URL 和模型不能为空');
      return;
    }
    let parsedUrl: URL;
    try {
      parsedUrl = new URL(normalizedUrl);
    } catch {
      setError('Base URL 必须是有效的 http 或 https 地址');
      return;
    }
    if (!['http:', 'https:'].includes(parsedUrl.protocol)) {
      setError('Base URL 必须是有效的 http 或 https 地址');
      return;
    }
    if (parsedUrl.username || parsedUrl.password || parsedUrl.search || parsedUrl.hash) {
      setError('Base URL 不能包含用户名、密码、查询参数或片段');
      return;
    }
    if (!runtime && !normalizedKey) {
      setError('首次配置必须填写 API Key');
      return;
    }
    const previousUrl = runtime?.baseUrl ? normalizePreviewUrl(runtime.baseUrl) : null;
    if (runtime && !normalizedKey && previousUrl && previousUrl.origin !== parsedUrl.origin) {
      setError('中转站来源已改变，请输入该来源对应的 API Key');
      return;
    }
    if (parsedUrl.protocol === 'http:' && !isLoopbackHost(parsedUrl.hostname)) {
      setError('远程中转站必须使用 HTTPS；HTTP 仅允许 localhost 或回环地址');
      return;
    }
    onSave({
      baseUrl: normalizedUrl,
      model: normalizedModel,
      apiKey: normalizedKey,
      transport,
    });
  }

  const previewUrl = normalizePreviewUrl(baseUrl);
  const insecureRemote = previewUrl?.protocol === 'http:' && !isLoopbackHost(previewUrl.hostname);
  const localError = insecureRemote
    ? '远程中转站必须使用 HTTPS；HTTP 仅允许 localhost 或回环地址'
    : error;
  const visibleError = localError ?? submitError?.message ?? null;
  const recoveryHint = localError ? null : relayFailureRecoveryHint(submitError);

  return (
      <section
        className="inline-config-panel relay-config-panel"
        aria-labelledby="relay-config-title"
        aria-describedby={`relay-config-note${visibleError ? ' relay-config-error' : ''}`}
      >
        <div className="card-title-row">
          <span className="section-icon"><KeyRound aria-hidden="true" /></span>
          <div>
            <p className="eyebrow">凭据受控输入</p>
            <h2 ref={headingRef} tabIndex={-1} id="relay-config-title">配置 API 中转站</h2>
          </div>
        </div>
        <form onSubmit={submit}>
          <label className="form-field">
            <span>Base URL</span>
            <input
              aria-label="Base URL"
              type="text"
              inputMode="url"
              value={baseUrl}
              onChange={(event) => setBaseUrl(event.target.value)}
              placeholder="https://your-relay.example.com/v1"
            />
          </label>
          <label className="form-field">
            <span>模型</span>
            <input aria-label="模型" value={model} onChange={(event) => setModel(event.target.value)} />
          </label>
          <fieldset className="relay-transport-fieldset">
            <legend>传输方式</legend>
            <label className={`relay-transport-option ${transport === 'http' ? 'selected' : ''}`}>
              <input
                type="radio"
                name="relay-transport"
                value="http"
                checked={transport === 'http'}
                onChange={() => setTransport('http')}
              />
              <span>
                <strong>HTTP Responses（推荐）</strong>
                <small>兼容大多数中转站；不会尝试 Responses WebSocket。</small>
              </span>
            </label>
            <label className={`relay-transport-option ${transport === 'websocket' ? 'selected' : ''}`}>
              <input
                type="radio"
                name="relay-transport"
                value="websocket"
                checked={transport === 'websocket'}
                onChange={() => setTransport('websocket')}
              />
              <span>
                <strong>Responses WebSocket</strong>
                <small>仅在服务商明确提供 Responses WSS 时选择；普通 HTTPS 接口不代表支持 WSS。</small>
              </span>
            </label>
          </fieldset>
          <label className="form-field">
            <span>API Key</span>
            <input
              aria-label="API Key"
              type="password"
              value={apiKey}
              onChange={(event) => setApiKey(event.target.value)}
              autoComplete="new-password"
              placeholder={runtime ? '留空则保留已加密保存的 Key' : '首次配置必填'}
            />
          </label>
          {visibleError ? <p className="form-error" id="relay-config-error" role="alert">{visibleError}</p> : null}
          {recoveryHint ? <p className="runtime-action-help">{recoveryHint}</p> : null}
          {!localError && submitError?.operationId ? (
            <DiagnosticExportAction operationId={submitError.operationId} />
          ) : null}
          <p className="safe-note" id="relay-config-note">仅支持 Bearer Key + Responses API。Key 仅提交给本机后端加密保存，不会回填；保存和切换不会验证服务连通性、模型或 WebSocket 能力。</p>
          <div className="form-actions">
            <button type="button" className="ghost-button inline" onClick={cancel} disabled={busy}>
              <X className="button-icon" aria-hidden="true" />
              取消
            </button>
            <button type="submit" className="primary-button" disabled={busy || insecureRemote}>
              <Save className="button-icon" aria-hidden="true" />
              保存中转站
            </button>
          </div>
        </form>
      </section>
  );
}

function normalizePreviewUrl(value: string) {
  const raw = value.trim();
  if (!raw) return null;
  try {
    return new URL(/^[a-z][a-z\d+.-]*:\/\//i.test(raw) ? raw : `https://${raw}`);
  } catch {
    return null;
  }
}

function isLoopbackHost(hostname: string) {
  return hostname === 'localhost' || hostname === '127.0.0.1' || hostname === '[::1]' || hostname === '::1';
}

function relayFailureRecoveryHint(failure: RelaySubmitFailure | null) {
  if (!failure) return null;
  if (failure.code === 'providerCapabilityInvalid' || failure.recoverability === 'reconfigure') {
    return '请按受支持合同重新填写：HTTPS（回环地址可 HTTP）、Bearer Key、Responses API 和明确的传输方式。';
  }
  if (failure.recoverability === 'closeWritersAndRetry') {
    return '请关闭 ChatGPT/Codex 写入进程后重试。';
  }
  if (failure.recoverability === 'unsupportedClient') {
    return '当前客户端结构未经支持验证；请保留现场并导出诊断。';
  }
  return null;
}
