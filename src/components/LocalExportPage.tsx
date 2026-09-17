import { Check, Download, FileDown, LoaderCircle } from "lucide-react";
import { useState } from "react";
import type { ExportFormat } from "../domain/types";
import { createLocalExport, type ServerExportResult } from "../lib/server-api";
import { ExportPresentationOptions } from "./ExportPresentationOptions";
import { ExportDirectoryButton } from "./ExportDirectoryButton";

interface LocalExportPageProps {
  token: string;
  onCompleted: (result: ServerExportResult) => void;
  onFailed: (error: unknown) => void;
}

export function LocalExportPage({ token, onCompleted, onFailed }: LocalExportPageProps) {
  const [format, setFormat] = useState<ExportFormat>("json");
  const [dataRedaction, setDataRedaction] = useState(true);
  const [simplify, setSimplify] = useState(true);
  const [pretty, setPretty] = useState(true);
  const [busy, setBusy] = useState(false);

  const submit = async () => {
    if (busy) return;
    setBusy(true);
    try {
      onCompleted(await createLocalExport(token, { format, dataRedaction, simplify, pretty }));
    } catch (error) {
      onFailed(error);
    } finally {
      setBusy(false);
    }
  };

  return <main className="full-page local-export-page">
    <section className="local-export-card">
      <div className="local-export-card-heading"><span className="local-export-icon"><FileDown size={22} /></span><div><h2>快速导出本机记录</h2><p>适合临时导出和离线分析，生成速度比“采集后再从服务端导出”更快。</p></div><ExportDirectoryButton token={token} onFailed={onFailed} /></div>
      <div className="local-export-options">
        <fieldset><legend>导出格式</legend><div className="format-grid">{(["json", "csv", "html", "pdf"] as ExportFormat[]).map((item) => <button className={format === item ? "selected" : ""} key={item} type="button" onClick={() => setFormat(item)}><span>{format === item && <Check size={15} />}{item.toUpperCase()}</span></button>)}</div></fieldset>
        <ExportPresentationOptions dataRedaction={dataRedaction} simplify={simplify} pretty={pretty} onDataRedactionChange={setDataRedaction} onSimplifyChange={setSimplify} onPrettyChange={setPretty} disabled={busy} />
      </div>
      <div className="local-export-actions"><p><Download size={15} />文件将保存到服务端默认导出目录，可在完成提示中直接打开。</p><button className="primary-button" type="button" disabled={busy} onClick={() => void submit()}>{busy ? <><LoaderCircle className="spin" size={17} />正在读取并导出…</> : <><Download size={17} />开始导出</>}</button></div>
    </section>
  </main>;
}
