import { useEffect, useState } from "react";
import type { ArchiveTaskProgress } from "../lib/long-task";

export function ArchiveTaskStatus({ progress, uncertain, checking, onRecheck, onDismiss }: {
  progress: ArchiveTaskProgress; uncertain: boolean; checking: boolean;
  onRecheck: () => void; onDismiss: () => void;
}) {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, []);
  const seconds = Math.max(0, Math.floor((now - Date.parse(progress.startedAt)) / 1000)) || 0;
  const elapsed = seconds >= 60 ? `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒` : `${seconds} 秒`;
  const label = progress.kind === "import" ? "导入" : "本机采集";
  return <aside className="archive-task-status" role="status" aria-label={`${label}任务状态`}>
    <strong>{label} · {uncertain && !checking ? "结果暂未确认" : progress.stage}</strong>
    <span>已等待 {elapsed}{progress.receivedBytes > 0 ? ` · 已接收 ${(progress.receivedBytes / 1024 / 1024).toFixed(2)} MiB` : ""}</span>
    {(uncertain || progress.trackingUnavailable) && <p>暂时无法确认最终结果，任务可能仍在运行。查询原任务不会重复执行。</p>}
    {uncertain && <div><button className="secondary-button" disabled={checking} onClick={onRecheck}>{checking ? "正在查询…" : "继续查询"}</button><button className="secondary-button" disabled={checking} onClick={onDismiss}>结束跟踪</button><small>结束跟踪只关闭状态显示；再次执行前请先核对归档结果。</small></div>}
  </aside>;
}
