import { useEffect, useState } from "react";
import { CheckCircle2, ChevronDown, FileDown, FolderOpen, HardDriveDownload, ListChecks, RefreshCw, Square, Upload } from "lucide-react";
import { cancelServerExport, getTaskHistory, openExportDirectory, type TaskHistoryPage, type TaskHistoryRecord } from "../lib/server-api";
import { isActive, time } from "./ArchiveHealthPage";

const kinds: Record<string, string> = { import: "归档导入", collection: "本机采集", collector_upload: "采集端上传", export: "归档导出", local_export: "本机导出", health_check: "归档校验" };
const statuses: Record<string, string> = { queued: "等待执行", receiving: "接收中", running: "执行中", cancelling: "正在取消", completed: "已完成", failed: "未完成", cancelled: "已取消", unknown: "结果待核对" };

export function TaskCenterPage({ token, superAdminEnabled }: { token: string; superAdminEnabled: boolean }) {
  const [page, setPage] = useState<TaskHistoryPage>();
  const [kind, setKind] = useState("");
  const [status, setStatus] = useState("");
  const [offset, setOffset] = useState(0);
  const [error, setError] = useState("");
  const [actionError, setActionError] = useState("");
  const [refresh, setRefresh] = useState(0);
  const [acting, setActing] = useState("");
  const [expandedId, setExpandedId] = useState<string>();
  useEffect(() => { setPage(undefined); setExpandedId(undefined); }, [token]);
  useEffect(() => {
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        const next = await getTaskHistory(token, { kind, status, offset }, controller.signal);
        if (controller.signal.aborted) return;
        setPage(next); setError("");
      } catch (reason) {
        if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : "任务历史读取失败。");
      } finally { if (!controller.signal.aborted) timer = setTimeout(() => void load(), 5000); }
    };
    void load();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [token, kind, status, offset, refresh]);
  const action = async (id: string, run: () => Promise<unknown>) => {
    setActing(id); setActionError("");
    try { await run(); setRefresh((value) => value + 1); }
    catch (reason) { setActionError(reason instanceof Error ? reason.message : "操作失败，请刷新任务状态。"); }
    finally { setActing(""); }
  };
  const resetView = () => { setPage(undefined); setExpandedId(undefined); };
  const changePage = (next: number) => { resetView(); setOffset(next); };
  return <main className="operations-page" aria-label="任务中心">
    <div className="operations-content">
      <header className="operations-heading operations-page-heading">
        <div className="operations-title"><h1>任务中心</h1>{page && <span className="operations-count">{page.total.toLocaleString("zh-CN")} 项</span>}</div>
        <div className="operations-actions"><span className="operations-live"><i />自动更新</span><button className="secondary-button" onClick={() => setRefresh((value) => value + 1)}><RefreshCw size={14} />刷新任务</button></div>
      </header>
      <div className="operations-filters">
        <label>任务类型<select value={kind} onChange={(event) => { resetView(); setKind(event.target.value); setOffset(0); }}><option value="">全部类型</option>{Object.entries(kinds).map(([key, name]) => <option value={key} key={key}>{name}</option>)}</select></label>
        <label>任务状态<select value={status} onChange={(event) => { resetView(); setStatus(event.target.value); setOffset(0); }}><option value="">全部状态</option>{Object.entries(statuses).map(([key, name]) => <option value={key} key={key}>{name}</option>)}</select></label>
      </div>
      {error && <p role="alert" className="operations-warning">{error}</p>}
      {actionError && <p role="alert" className="operations-warning">{actionError}</p>}
      {page?.persistenceError && <p role="alert" className="operations-warning">任务记录保存失败，当前结果可能尚未保存。请检查磁盘空间和目录权限。</p>}
      {!page && !error && <div className="operations-empty" role="status"><RefreshCw className="spin" size={18} />正在读取任务历史…</div>}
      {page && <>
        {page.records.length === 0 ? <div className="operations-empty"><ListChecks size={28} /><span>暂无符合条件的任务</span></div> : <div className="task-history-list">
          <div className="task-list-heading" aria-hidden="true"><span>任务 / 结果</span><span>开始时间</span><span>状态</span><span /></div>
          {page.records.map((record) => {
            const expanded = expandedId === record.taskId;
            const Icon = record.kind === "collection" ? HardDriveDownload : record.kind === "collector_upload" ? Upload : ["export", "local_export"].includes(record.kind) ? FileDown : record.kind === "health_check" ? CheckCircle2 : Upload;
            const canOpenOutput = superAdminEnabled && (record.output || record.status === "unknown" && ["export", "local_export"].includes(record.kind));
            return <article className={"task-row" + (expanded ? " expanded" : "")} key={record.taskId} data-task-id={record.taskId}>
              <div className="task-row-main">
                <div className="task-row-identity"><span className="task-kind-icon"><Icon size={17} /></span><div><h2>{kinds[record.kind] ?? "归档任务"}</h2><span className="task-result-summary">{resultSummary(record)}</span></div></div>
                <time className="task-row-time" dateTime={record.startedAt} title={time(record.startedAt)}>{time(record.startedAt, true)}</time>
                <span className={"task-state task-state-" + record.status}>{statuses[record.status] ?? "状态未知"}</span>
                <div className="task-row-actions">
                  {canOpenOutput && <button className="operations-icon-button" aria-label="打开导出目录" title="打开导出目录" disabled={Boolean(acting)} onClick={() => void action(record.taskId, () => openExportDirectory(token))}><FolderOpen size={16} /></button>}
                  {superAdminEnabled && record.kind === "export" && isActive(record) && <button className="operations-icon-button" aria-label="取消导出" title="取消导出" disabled={Boolean(acting)} onClick={() => void action(record.taskId, () => cancelServerExport(token, record.taskId))}><Square size={14} /></button>}
                  <button className="operations-icon-button task-expand-button" aria-label={expanded ? "收起任务详情" : "展开任务详情"} aria-expanded={expanded} aria-controls={"task-detail-" + record.taskId} title={expanded ? "收起详情" : "任务详情"} onClick={() => setExpandedId(expanded ? undefined : record.taskId)}><ChevronDown size={17} /></button>
                </div>
              </div>
              {expanded && <div className="task-row-details" id={"task-detail-" + record.taskId}>
                {record.status === "unknown" && <p className="operations-warning">结果尚未确认，请先核对归档或导出文件。此任务不会自动重跑。</p>}
                <dl className="operations-details">
                  <dt>任务标识</dt><dd className="task-identifier">{record.taskId}</dd>
                  <dt>开始时间</dt><dd>{time(record.startedAt)}</dd>
                  <dt>最近更新</dt><dd>{time(record.updatedAt)}</dd>
                  {record.phase && <><dt>最近阶段</dt><dd>{record.phase}</dd></>}
                  {record.errorCode && <><dt>错误码</dt><dd>{record.errorCode}</dd></>}
                  {record.output && <><dt>导出文件</dt><dd>{record.output.fileName}</dd><dt>消息 / 媒体</dt><dd>{record.output.messageCount} 条 / {record.output.mediaCount} 项</dd><dt>缺失媒体</dt><dd>{record.output.missingMediaCount} 项</dd></>}
                  {record.importResult && <><dt>新增 / 修订 / 未变化</dt><dd>{record.importResult.inserted} / {record.importResult.revised} / {record.importResult.unchanged} 条</dd><dt>缺失媒体</dt><dd>{record.importResult.missingMediaCount} 项</dd></>}
                  {record.verification && <><dt>归档文件缺失</dt><dd>{record.verification.missingFiles} 项</dd><dt>内容损坏</dt><dd>{record.verification.damagedFiles} 项</dd><dt>无法读取</dt><dd>{record.verification.unreadableFiles} 项</dd></>}
                  {record.receivedBytes > 0 && <><dt>接收检查点</dt><dd>{(record.receivedBytes / 1024 / 1024).toFixed(2)} MiB</dd></>}
                </dl>
              </div>}
            </article>;
          })}
        </div>}
        {page.total > 50 && <div className="operations-pagination"><button className="secondary-button" disabled={offset === 0} onClick={() => changePage(Math.max(0, offset - 50))}>上一页</button><span>{Math.floor(offset / 50) + 1} / {Math.ceil(page.total / 50)}</span><button className="secondary-button" disabled={offset + 50 >= page.total} onClick={() => changePage(offset + 50)}>下一页</button></div>}
      </>}
      <details className="operations-disclosure operations-help"><summary>状态说明</summary><p>历史会在重启后保留。结果待核对的任务不会自动重跑，请先核对归档或导出文件。</p></details>
    </div>
  </main>;
}

function resultSummary(record: TaskHistoryRecord): string {
  if (record.importResult) {
    const result = record.importResult;
    const changes = [result.inserted > 0 ? "新增 " + result.inserted + " 条" : "", result.revised > 0 ? "修订 " + result.revised + " 条" : ""].filter(Boolean);
    return (changes.join(" · ") || "无内容变化") + (result.missingMediaCount > 0 ? " · " + result.missingMediaCount + " 项媒体缺失" : "");
  }
  if (record.output) return "已导出 " + record.output.messageCount + " 条消息" + (record.output.missingMediaCount > 0 ? " · " + record.output.missingMediaCount + " 项媒体缺失" : "");
  if (record.verification) {
    const result = record.verification;
    if (!result.databaseOk) return "数据库检查未通过";
    const issues = result.missingFiles + result.damagedFiles + result.unreadableFiles;
    return issues > 0 ? "已检查 " + result.checkedMedia + " 项附件 · " + issues + " 项异常" : "数据库正常 · 已检查 " + result.checkedMedia + " 项附件";
  }
  if (record.status === "unknown") return "需核对归档或导出文件";
  if (record.status === "failed") return "未完成" + (record.phase ? " · " + record.phase : "");
  return record.phase || statuses[record.status] || "等待更新";
}
