export type OperationView = {
  label: string;
  operationId?: string;
  metrics: string[];
  backupCount?: number;
  backupPaths?: string[];
  rolledBack?: boolean;
  warnings?: string[];
};

export function OperationResultPanel({ result }: { result: OperationView }) {
  return (
    <section className={`operation-result ${result.rolledBack ? 'rolled-back' : ''}`} aria-live="polite">
      <div>
        <p className="eyebrow">最近操作回执</p>
        <h2>{result.label}</h2>
      </div>
      <div className="receipt-items">
        {result.operationId ? <span>操作 ID：{result.operationId}</span> : null}
        {result.metrics.map((metric) => <span key={metric}>{metric}</span>)}
        {result.backupCount !== undefined ? <span>备份：{result.backupCount}</span> : null}
        {result.backupPaths?.map((path) => <span className="receipt-path" title={path} key={path}>备份路径：{path}</span>)}
        {result.warnings?.map((warning) => <span className="receipt-warning" key={warning}>警告：{warning}</span>)}
        {result.rolledBack ? <span>结果：已回滚</span> : <span>结果：成功</span>}
      </div>
    </section>
  );
}
