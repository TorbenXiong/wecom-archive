import { useEffect, useState } from "react";
import { ArrowUpRight, CircleHelp, LoaderCircle, RefreshCw, ShieldCheck, TriangleAlert } from "lucide-react";
import { getArchiveHealth, startArchiveHealthCheck, ServerApiError, type ArchiveHealth, type TaskHistoryRecord } from "../lib/server-api";

export function ArchiveHealthPage({ token, onOpenTasks }: { token: string; onOpenTasks: () => void }) {
  const [health, setHealth] = useState<ArchiveHealth>();
  const [error, setError] = useState("");
  const [startError, setStartError] = useState("");
  const [busy, setBusy] = useState(false);
  const [refresh, setRefresh] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        const next = await getArchiveHealth(token, controller.signal);
        if (controller.signal.aborted) return;
        setHealth(next); setError("");
        if (next.latestVerification && isActive(next.latestVerification)) timer = setTimeout(() => void load(), 3000);
      } catch (reason) {
        if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : "无法读取归档状态。");
      }
    };
    void load();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [token, refresh]);
  const run = async () => {
    setBusy(true); setStartError("");
    try { await startArchiveHealthCheck(token); }
    catch (reason) {
      setStartError(reason instanceof ServerApiError ? reason.message : "校验状态未确认，请先在任务中心核对。");
    } finally { setBusy(false); setRefresh((value) => value + 1); }
  };
  const latest = health?.latestVerification;
  const result = latest?.verification;
  const running = Boolean(latest && isActive(latest));
  const stale = Boolean(result && result.revision !== health?.revision);
  const fileIssues = result ? result.missingFiles + result.damagedFiles + result.unreadableFiles : 0;
  const tone = running || stale ? "neutral" : result ? !result.databaseOk || fileIssues > 0 ? "warning" : "success" : latest ? "warning" : "neutral";
  const StatusIcon = running ? LoaderCircle : tone === "success" ? ShieldCheck : tone === "warning" ? TriangleAlert : CircleHelp;
  const title = running ? "正在校验" : result ? !result.databaseOk ? "发现数据库异常" : fileIssues > 0 ? "发现 " + fileIssues + " 项附件异常" : stale ? "归档有更新，建议重新校验" : "最近校验通过" : latest?.status === "unknown" ? "上次校验结果待核对" : latest?.status === "failed" ? "上次校验未完成" : "尚未执行完整校验";
  const subtitle = running ? "可离开此页，结果将自动保存" : result ? (result.databaseOk ? "数据库正常 · " : "") + "已检查 " + result.checkedMedia + " 项归档附件" + (stale && (!result.databaseOk || fileIssues > 0) ? " · 归档已有更新" : "") : latest ? "可查看任务记录，或重新校验" : "检查数据库与已采集附件的完整性";

  return <main className="operations-page" aria-label="归档健康">
    <div className="operations-content">
      <header className="operations-heading operations-page-heading">
        <h1>归档健康</h1>
        <div className="operations-actions">
          <button className="secondary-button" onClick={() => setRefresh((value) => value + 1)}><RefreshCw size={14} />刷新状态</button>
          <button className="primary-button" disabled={busy || running} onClick={() => void run()}>{busy || running ? <LoaderCircle className="spin" size={15} /> : <ShieldCheck size={15} />}{busy ? "正在登记…" : running ? "校验中…" : "开始完整校验"}</button>
        </div>
      </header>
      {error && <p role="alert" className="operations-warning">{error}</p>}
      {startError && <p role="alert" className="operations-warning">{startError}</p>}
      {!health && !error && <div className="operations-empty" role="status"><LoaderCircle className="spin" size={20} />正在读取归档状态…</div>}
      {health && <>
        <section className={"health-check-status health-check-" + tone} aria-label="校验状态">
          <div className="health-check-overview" role="status">
            <span className="health-check-icon"><StatusIcon className={running ? "spin" : undefined} size={25} /></span>
            <div><h2 className="health-check-title">{title}</h2><p>{subtitle}</p></div>
          </div>
          <div className="health-check-meta">
            {latest && <span>{time(latest.startedAt, true)}</span>}
            <button className="operations-text-button" onClick={onOpenTasks}>查看任务中心<ArrowUpRight size={14} /></button>
          </div>
          {result && <details className="operations-disclosure health-check-details">
            <summary>校验详情</summary>
            <dl className="operations-details">
              <dt>数据库</dt><dd className={result.databaseOk ? "operations-success" : "operations-warning-text"}>{result.databaseOk ? "数据库完整性检查通过" : "数据库完整性检查未通过"}</dd>
              <dt>已检查附件</dt><dd>{result.checkedMedia} 项</dd>
              <dt>归档文件缺失</dt><dd>{result.missingFiles} 项</dd>
              <dt>大小或哈希不符</dt><dd>{result.damagedFiles} 项</dd>
              <dt>无法读取</dt><dd>{result.unreadableFiles} 项</dd>
            </dl>
          </details>}
        </section>
        <div className="health-metrics health-primary-metrics">
          <Metric label="已归档消息" value={health.messageCount} />
          <Metric label="会话" value={health.conversationCount} />
          <Metric label="数据源" value={health.sourceCount} />
          <Metric label="采集批次" value={health.batchCount} />
        </div>
        <div className="health-overview-grid">
          <section className="operations-card">
            <h2>归档概览</h2>
            <dl className="operations-details health-archive-facts">
              <dt>消息范围</dt><dd className="health-date-range"><span>{time(health.firstMessageAt, true)}</span><span className="operations-muted">至</span><span>{time(health.lastMessageAt, true)}</span></dd>
              <dt>最近采集</dt><dd>{time(health.lastCollectedAt, true)}</dd>
              <dt>数据库大小</dt><dd title="仅数据库主文件，不含 WAL 和媒体">{bytes(health.databaseBytes)}</dd>
              <dt>磁盘可用</dt><dd>{bytes(health.availableBytes)}</dd>
            </dl>
            <details className="operations-disclosure">
              <summary>采集详情{health.unsupportedMessages > 0 && <span className="operations-warning-text"> · {health.unsupportedMessages} 条暂不支持</span>}</summary>
              <dl className="operations-details"><dt>限定范围的批次</dt><dd>{health.restrictedBatches} 个</dd><dt>不支持的消息</dt><dd>{health.unsupportedMessages} 条</dd></dl>
            </details>
          </section>
          <section className="operations-card">
            <h2>媒体概况</h2>
            <div className="health-metrics health-media-metrics">
              <Metric label="已采集" value={health.verifiedMedia} />
              <Metric label="采集时缺失" value={health.missingMedia} warning={health.missingMedia > 0} />
              <Metric label="采集时异常" value={health.invalidMedia} warning={health.invalidMedia > 0} />
            </div>
            {health.missingMedia > 0 && <details className="operations-disclosure">
              <summary>查看缺失原因</summary>
              <dl className="operations-details"><dt>未解析到附件位置</dt><dd>{health.missingLocator} 项</dd><dt>授权目录未找到</dt><dd>{health.missingInSource} 项</dd><dt>其他原因</dt><dd>{health.missingOther} 项</dd></dl>
              <p className="operations-muted">未找到可能是尚未缓存，不代表源文件已删除。</p>
            </details>}
          </section>
        </div>
        <footer className="operations-footer">
          <details className="operations-disclosure operations-help"><summary>检查范围</summary><p>仅检查已归档内容及已采集附件，不代表源端记录完整。消息时间空档不作为漏采依据；校验不补采或修复文件。</p></details>
          <span className="operations-muted">更新于 {time(health.checkedAt, true)}</span>
        </footer>
      </>}
    </div>
  </main>;
}

function Metric({ label, value, warning }: { label: string; value: number; warning?: boolean }) {
  return <div className={"health-metric" + (warning ? " health-metric-warning" : "")}><span>{label}</span><strong>{value.toLocaleString("zh-CN")}</strong></div>;
}
export function time(value?: string | null, compact = false): string {
  if (!value) return "暂无记录";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? "时间未知" : date.toLocaleString("zh-CN", compact
    ? { year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false }
    : { hour12: false });
}
export function isActive(record: TaskHistoryRecord): boolean { return ["queued", "receiving", "running", "cancelling"].includes(record.status); }
function bytes(value?: number | null): string { return value == null ? "暂不可用" : (value / 1024 / 1024 / 1024).toFixed(2) + " GiB"; }
