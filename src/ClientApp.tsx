import { CheckCircle2, Download, LoaderCircle, RefreshCw, Upload } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { InfoDialog } from "./components/Overlays";
import type { CollectionSchedule } from "./domain/types";
import { backend, formatBackendError, type CollectionSummary, type CollectorScheduleStatus } from "./lib/backend";
import "./styles.css";

type PrepareState = "preparing" | "ready" | "failed";
type ClientInfo = { title: string; message: string; tone: "success" | "error"; openDirectory?: boolean };

function describeSchedule(schedule: CollectionSchedule | undefined): string {
  if (!schedule || schedule.mode === "disabled") return "后台计划未启用";
  if (schedule.mode === "daily") return `后台每天 ${schedule.dailyTime} 自动采集并上传`;
  return `后台每 ${schedule.intervalMinutes} 分钟自动采集并上传`;
}

export default function ClientApp() {
  const [state, setState] = useState<PrepareState>("preparing");
  const [summary, setSummary] = useState<CollectionSummary>();
  const [status, setStatus] = useState("正在准备…");
  const [uploading, setUploading] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [info, setInfo] = useState<ClientInfo>();
  const [displayName, setDisplayName] = useState("采集端");
  const [offlineExportEnabled, setOfflineExportEnabled] = useState(false);
  const [scheduleDescription, setScheduleDescription] = useState("后台计划未启用");
  const [scheduleStatus, setScheduleStatus] = useState<CollectorScheduleStatus>({ running: false });
  const [offlineExportPrompt, setOfflineExportPrompt] = useState(false);

  const prepare = useCallback(async () => {
    setState("preparing");
    setSummary(undefined);
    setStatus("正在准备…");
    try {
      const bootstrap = await backend.bootstrap();
      setDisplayName(bootstrap.displayName || `${bootstrap.organizationName || "组织"}采集端`);
      setOfflineExportEnabled(bootstrap.offlineExportEnabled ?? false);
      setScheduleDescription(describeSchedule(bootstrap.collectorSchedule));
      const sources = await backend.discoverSources();
      const source = sources[0];
      if (!source) throw new Error("未发现可支持的本机企业微信数据，请确认客户端已登录。");
      const result = await backend.collectSourceAutomatically(source.sourceId);
      setSummary(result);
      if (bootstrap.collectorSchedule && bootstrap.collectorSchedule.mode !== "disabled") {
        try {
          await backend.uploadLatest();
          setStatus("采集端已完成首次自动采集并上传，之后将按计划自动采集并上传，无需手动操作。");
        } catch (reason) {
          setStatus(`采集已准备，但首次自动上传失败：${formatBackendError(reason, "请检查服务端连接，后台计划会继续重试。")}`);
        } finally {
          await backend.hideCollectorWindow();
        }
      } else {
        setStatus("本机数据已采集，可手动上传或导出。");
      }
      setState("ready");
    } catch (reason) {
      const message = formatBackendError(reason, "自动解析未完成，请保持企业微信运行后重试。");
      setStatus(message);
      setState("failed");
    }
  }, []);

  useEffect(() => { void prepare(); }, [prepare]);
  useEffect(() => {
    let active = true;
    const refresh = async () => {
      try {
        const next = await backend.getCollectorScheduleStatus();
        if (!active) return;
        setScheduleStatus(next);
        if (next.lastMessageCount !== undefined) {
          setSummary((current) => current ? {
            ...current,
            messageCount: next.lastMessageCount!,
            mediaCount: next.lastMediaCount ?? current.mediaCount,
          } : current);
        }
      } catch { /* 主操作状态仍会显示可恢复错误。 */ }
    };
    void refresh();
    const timer = window.setInterval(() => void refresh(), 5000);
    return () => { active = false; window.clearInterval(timer); };
  }, []);
  useEffect(() => {
    if (!backend.isNative()) return;
    let active = true;
    let unlisten: (() => void) | undefined;
    void listen("collector:open-offline-export", () => {
      if (active && offlineExportEnabled) setOfflineExportPrompt(true);
    }).then((dispose) => {
      if (active) unlisten = dispose;
      else dispose();
    });
    return () => { active = false; unlisten?.(); };
  }, [offlineExportEnabled]);

  const uploadCollection = async () => {
    if (state !== "ready" || uploading || exporting) return;
    setUploading(true);
    try {
      const result = await backend.uploadLatest();
      setInfo({ title: "上传完成", message: `已直接上传到归档工作台\n共 ${result.messageCount.toLocaleString("zh-CN")} 条消息。`, tone: "success" });
    } catch (reason) {
      setInfo({ title: "上传失败", message: formatBackendError(reason, "上传失败，临时文件已清理。"), tone: "error" });
    } finally { setUploading(false); }
  };

  const exportCollection = async () => {
    setOfflineExportPrompt(false);
    if (state !== "ready" || uploading || exporting || !offlineExportEnabled) return;
    setExporting(true);
    try {
      const result = await backend.exportLatestEncrypted();
      setInfo({ title: "导出完成", message: `加密文件已生成：${result.fileName}\n共 ${result.messageCount.toLocaleString("zh-CN")} 条消息。`, tone: "success", openDirectory: true });
    } catch (reason) {
      setInfo({ title: "导出失败", message: formatBackendError(reason, "加密文件导出失败。"), tone: "error" });
    } finally { setExporting(false); }
  };

  return <div className="collector-shell">
    <main className="collector-main"><section className={`collector-card ${state}`}>
      <span className="collector-state-icon">{state === "preparing" && <LoaderCircle className="spin" />}{state === "ready" && <CheckCircle2 />}{state === "failed" && <RefreshCw />}</span>
      <h1>{state === "preparing" ? "正在采集本机数据" : state === "ready" ? displayName : "采集未完成"}</h1>
      <p>{status}</p><small className="collector-schedule-status">{scheduleDescription}。关闭窗口后仍在托盘运行；左键托盘图标可打开{offlineExportEnabled ? "离线导出" : "采集端"}，右键可退出。{scheduleStatus.running ? "后台采集正在执行。" : scheduleStatus.lastError ? `最近执行失败：${scheduleStatus.lastError}` : scheduleStatus.lastSuccessAt ? `最近成功：${new Date(scheduleStatus.lastSuccessAt).toLocaleString("zh-CN", { hour12: false })}` : ""}</small>
      {state === "ready" && <div className="collector-result"><strong>{summary?.messageCount.toLocaleString("zh-CN")}</strong><span>条消息</span><i /><span>{summary?.mediaCount.toLocaleString("zh-CN")} 项媒体引用</span></div>}
      {state === "failed" ? <button className="primary-button collector-main-button" type="button" onClick={() => void prepare()}><RefreshCw size={17} />重试</button> : <div className={offlineExportEnabled ? "collector-action-grid" : "collector-action-grid single"}><button className="primary-button collector-main-button" disabled={state !== "ready" || uploading || exporting} type="button" onClick={uploadCollection}>{uploading ? <><LoaderCircle className="spin" size={17} />正在上传…</> : <><Upload size={17} />上传到归档工作台</>}</button>{offlineExportEnabled && <button className="secondary-button collector-main-button" disabled={state !== "ready" || uploading || exporting} type="button" onClick={() => void exportCollection()}>{exporting ? <><LoaderCircle className="spin" size={17} />正在导出…</> : <><Download size={17} />导出加密文件</>}</button>}</div>}
    </section></main>
    {info && <InfoDialog title={info.title} tone={info.tone} actionLabel={info.openDirectory ? "打开目录" : undefined} onAction={info.openDirectory ? () => void backend.openOfflineExportDirectory() : undefined} onConfirm={() => setInfo(undefined)}>{info.message}</InfoDialog>}
    {offlineExportPrompt && <div className="modal-backdrop" role="presentation"><section className="modal-card collector-offline-export-dialog" role="dialog" aria-modal="true" aria-labelledby="collector-offline-export-title"><span className="modal-icon"><Download /></span><h2 id="collector-offline-export-title">离线导出</h2><p>将当前已采集数据导出为加密文件，供管理员稍后导入归档工作台。</p><div className="modal-actions"><button className="secondary-button" type="button" onClick={() => setOfflineExportPrompt(false)}>取消</button><button className="primary-button" type="button" onClick={() => void exportCollection()} disabled={state !== "ready" || uploading || exporting}>{exporting ? "正在导出…" : "开始导出"}</button></div></section></div>}
  </div>;
}
