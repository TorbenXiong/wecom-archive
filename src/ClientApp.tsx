import { CheckCircle2, Download, LoaderCircle, RefreshCw } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { InfoDialog } from "./components/Overlays";
import { backend, formatBackendError, type ClientExportFormat, type CollectionSummary } from "./lib/backend";
import "./styles.css";

type PrepareState = "preparing" | "ready" | "failed";
type ClientInfo = { title: string; message: string; canOpenFolder?: boolean };
const exportFormats: ClientExportFormat[] = ["json", "csv", "html", "txt"];

export default function ClientApp() {
  const [state, setState] = useState<PrepareState>("preparing");
  const [summary, setSummary] = useState<CollectionSummary>();
  const [status, setStatus] = useState("正在准备，仅处理您有权归档的数据…");
  const [exporting, setExporting] = useState(false);
  const [format, setFormat] = useState<ClientExportFormat>("json");
  const [info, setInfo] = useState<ClientInfo>();

  const prepare = useCallback(async () => {
    setState("preparing");
    setSummary(undefined);
    setStatus("正在准备，仅处理您有权归档的数据…");
    try {
      const sources = await backend.discoverSources();
      const source = sources[0];
      if (!source) throw new Error("未发现可支持的本机企业微信数据，请确认客户端已登录。");
      const result = await backend.collectSourceAutomatically(source.sourceId);
      setSummary(result);
      setStatus("准备完成。");
      setState("ready");
    } catch (reason) {
      setStatus(formatBackendError(reason, "自动解析未完成，请保持企业微信运行后重试。"));
      setState("failed");
    }
  }, []);

  useEffect(() => { void prepare(); }, [prepare]);

  const exportCollection = async () => {
    if (state !== "ready" || exporting) return;
    setExporting(true);
    try {
      const selection = await backend.pickDirectory("export");
      if (!selection) return;
      const result = await backend.exportLatest(format, selection.handle);
      setInfo({ title: "导出完成", message: `${result.fileName}\n共 ${result.messageCount.toLocaleString("zh-CN")} 条消息。`, canOpenFolder: true });
    } catch (reason) {
      setInfo({ title: "导出失败", message: formatBackendError(reason, "导出失败，未完成文件已清理。") });
    } finally { setExporting(false); }
  };

  const openExportFolder = async () => {
    try {
      await backend.openExportFolder();
    } catch (reason) {
      setInfo({ title: "打开文件夹失败", message: formatBackendError(reason, "无法打开导出文件夹。") });
    }
  };

  return <div className="simple-client-shell">
    <main className="simple-client-main"><section className={`simple-export-card ${state}`}>
      <span className="simple-state-icon">{state === "preparing" && <LoaderCircle className="spin" />}{state === "ready" && <CheckCircle2 />}{state === "failed" && <RefreshCw />}</span>
      <h1>{state === "preparing" ? "正在准备" : state === "ready" ? "可以导出" : "自动解析未完成"}</h1>
      <p>{status}</p>
      {state === "ready" && <div className="simple-result"><strong>{summary?.messageCount.toLocaleString("zh-CN")}</strong><span>条消息</span><i /><span>{summary?.mediaCount.toLocaleString("zh-CN")} 项媒体引用</span></div>}
      {state === "ready" && <fieldset className="simple-format-field" disabled={exporting}>
        <legend>导出格式</legend>
        <div className="simple-format-options" role="radiogroup" aria-label="导出格式">
          {exportFormats.map((item) => <label className="simple-format-option" key={item}>
            <input type="radio" name="client-export-format" value={item} checked={format === item} onChange={() => setFormat(item)} />
            <span>{item.toUpperCase()}</span>
          </label>)}
        </div>
      </fieldset>}
      {state === "failed" ? <button className="primary-button simple-main-button" type="button" onClick={() => void prepare()}><RefreshCw size={17} />重试</button> : <button className="primary-button simple-main-button" disabled={state !== "ready" || exporting} type="button" onClick={exportCollection}>{exporting ? <><LoaderCircle className="spin" size={17} />正在导出…</> : <><Download size={17} />导出</>}</button>}
    </section></main>
    {info && <InfoDialog title={info.title} tone={info.canOpenFolder ? "success" : "error"} actionLabel={info.canOpenFolder ? "打开文件夹" : undefined} onAction={info.canOpenFolder ? () => void openExportFolder() : undefined} onConfirm={() => setInfo(undefined)}>{info.message}</InfoDialog>}
  </div>;
}
