import { Check, Download, FileDown, LoaderCircle } from "lucide-react";
import { useState } from "react";
import type { ExportFormat } from "../domain/types";
import { createLocalExport, type ServerExportResult } from "../lib/server-api";

interface LocalExportPageProps {
  token: string;
  onCompleted: (result: ServerExportResult) => void;
  onFailed: (error: unknown) => void;
}

export function LocalExportPage({ token, onCompleted, onFailed }: LocalExportPageProps) {
  const [format, setFormat] = useState<ExportFormat>("json");
  const [dataRedaction, setDataRedaction] = useState(false);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    if (busy) return;
    setBusy(true);
    try {
      onCompleted(await createLocalExport(token, { format, dataRedaction }));
    } catch (error) {
      onFailed(error);
    } finally {
      setBusy(false);
    }
  };

  return <main className="full-page local-export-page">
    <section className="local-export-card">
      <div className="local-export-card-heading"><span className="local-export-icon"><FileDown size={22} /></span><div><h2>快速导出本机记录</h2><p>适合临时导出和离线分析，生成速度比“采集后再从服务端导出”更快。</p></div></div>
      <div className="local-export-options">
        <fieldset><legend>导出格式</legend><div className="format-grid">{(["json", "csv", "html", "pdf"] as ExportFormat[]).map((item) => <button className={format === item ? "selected" : ""} key={item} type="button" onClick={() => setFormat(item)}><span>{format === item && <Check size={15} />}{item.toUpperCase()}</span></button>)}</div></fieldset>
        <label className="export-data-redaction-option" title="导出前脱敏账号、密码、令牌、联系方式及疑似凭据片段；文件名和图片名保持原样。"><input type="checkbox" checked={dataRedaction} onChange={(event) => setDataRedaction(event.target.checked)} />数据脱敏</label>
      </div>
      <div className="local-export-actions"><p><Download size={15} />文件将保存到服务端默认导出目录，可在完成提示中直接打开。</p><button className="primary-button" type="button" disabled={busy} onClick={() => void submit()}>{busy ? <><LoaderCircle className="spin" size={17} />正在读取并导出…</> : <><Download size={17} />开始导出</>}</button></div>
    </section>
  </main>;
}
