import { useEffect, useRef, useState } from 'react';
import type { RelayRuntimeInput, RuntimeMetadata } from './types';

type RelayRuntimeDialogProps = {
  runtime: RuntimeMetadata | null;
  fallbackModel: string;
  busy: boolean;
  submitError: string | null;
  onCancel: () => void;
  onSave: (input: RelayRuntimeInput) => void | Promise<unknown>;
};

export function RelayRuntimeDialog({ runtime, fallbackModel, busy, submitError, onCancel, onSave }: RelayRuntimeDialogProps) {
  const [baseUrl, setBaseUrl] = useState(runtime?.baseUrl ?? '');
  const [model, setModel] = useState(runtime?.model ?? fallbackModel);
  const [apiKey, setApiKey] = useState('');
  const [error, setError] = useState<string | null>(null);
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
      previousFocus?.focus();
    };
  }, []);

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
    if (parsedUrl.protocol === 'http:' && !isLoopbackHost(parsedUrl.hostname)
      && !window.confirm('该地址会通过明文 HTTP 发送 Bearer API Key，存在凭据泄露风险。仍要保存吗？')) {
      return;
    }
    onSave({
      baseUrl: normalizedUrl,
      model: normalizedModel,
      apiKey: normalizedKey,
    });
  }

  const previewUrl = normalizePreviewUrl(baseUrl);
  const insecureRemote = previewUrl?.protocol === 'http:' && !isLoopbackHost(previewUrl.hostname);
  const visibleError = error ?? submitError;

  return (
      <dialog
        ref={dialogRef}
        className="relay-dialog"
        aria-labelledby="relay-dialog-title"
        aria-describedby="relay-dialog-note relay-dialog-error"
        onCancel={(event) => { event.preventDefault(); if (!busy) onCancel(); }}
      >
        <div className="card-title-row">
          <span className="card-icon">🔑</span>
          <div><p className="eyebrow">凭据受控输入</p><h2 id="relay-dialog-title">配置 API 中转站</h2></div>
        </div>
        <form onSubmit={submit}>
          <label className="dialog-field">
            <span>Base URL</span>
            <input
              aria-label="Base URL"
              type="text"
              inputMode="url"
              value={baseUrl}
              onChange={(event) => setBaseUrl(event.target.value)}
              placeholder="https://your-relay.example.com/v1"
              autoFocus
            />
          </label>
          <label className="dialog-field">
            <span>模型</span>
            <input aria-label="模型" value={model} onChange={(event) => setModel(event.target.value)} />
          </label>
          <label className="dialog-field">
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
          {insecureRemote ? <p className="form-warning" role="status">警告：非本机 HTTP 地址会明文传输 Bearer API Key。</p> : null}
          {visibleError ? <p className="form-error" id="relay-dialog-error" role="alert">{visibleError}</p> : <span id="relay-dialog-error" />}
          <p className="safe-note" id="relay-dialog-note">Key 仅提交给本机后端加密保存，不会回填或显示在页面中。未写协议时自动使用 https://。</p>
          <div className="dialog-actions">
            <button type="button" className="ghost-button inline" onClick={onCancel} disabled={busy}>取消</button>
            <button type="submit" className="primary-button" disabled={busy}>保存中转站</button>
          </div>
        </form>
      </dialog>
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
