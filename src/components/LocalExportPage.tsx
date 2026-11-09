import { Check, Download, FileDown, LoaderCircle } from "lucide-react";
import { useState } from "react";
import type { ExportFormat } from "../domain/types";
import { createLocalExport, ServerApiError, type ExportOrder, type ServerExportResult } from "../lib/server-api";
import { ExportPresentationOptions } from "./ExportPresentationOptions";
import { ExportOrderOptions } from "./ExportOrderOptions";
import { ExportDirectoryButton } from "./ExportDirectoryButton";
import { CustomSourceRootDialog } from "./Overlays";

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
  const [conversationOrder, setConversationOrder] = useState<ExportOrder>("descending");
  const [messageOrder, setMessageOrder] = useState<ExportOrder>("descending");
  const [busy, setBusy] = useState(false);
  const [customRootIssue, setCustomRootIssue] = useState<{ message: string; attemptedRoot?: string } | null>(null);

  const submit = async (sourceRoot?: string) => {
    if (busy) return;
    setBusy(true);
    try {
      onCompleted(await createLocalExport(token, { format, sourceRoot, dataRedaction, simplify, pretty, conversationOrder, messageOrder }));
      setCustomRootIssue(null);
    } catch (error) {
      onFailed(error);
      if (error instanceof ServerApiError && error.code === "LOCAL_SOURCE_NOT_FOUND") {
        setCustomRootIssue({ message: error.message, attemptedRoot: sourceRoot });
      } else {
        setCustomRootIssue(null);
      }
    } finally {
      setBusy(false);
    }
  };

  return <main className="full-page local-export-page">
    <section className="local-export-card">
      <div className="local-export-card-heading"><span className="local-export-icon"><FileDown size={22} /></span><div><h2>快速导出本机记录</h2></div><ExportDirectoryButton token={token} onFailed={onFailed} /></div>
      <div className="local-export-options">
        <fieldset><legend>导出格式</legend><div className="format-grid">{(["json", "csv", "html", "md"] as ExportFormat[]).map((item) => <button className={format === item ? "selected" : ""} key={item} type="button" onClick={() => setFormat(item)}><span>{format === item && <Check size={15} />}{item.toUpperCase()}</span></button>)}</div></fieldset>
        <div className="local-export-config-row">
          <fieldset className="local-export-config-options"><legend>导出配置</legend><ExportPresentationOptions dataRedaction={dataRedaction} simplify={simplify} pretty={pretty} onDataRedactionChange={setDataRedaction} onSimplifyChange={setSimplify} onPrettyChange={setPretty} disabled={busy} /></fieldset>
          <ExportOrderOptions conversationOrder={conversationOrder} messageOrder={messageOrder} onConversationOrderChange={setConversationOrder} onMessageOrderChange={setMessageOrder} disabled={busy} />
        </div>
      </div>
      <div className="local-export-actions"><button className="primary-button" type="button" disabled={busy} onClick={() => void submit()}>{busy ? <><LoaderCircle className="spin" size={17} />正在读取并导出…</> : <><Download size={17} />开始导出</>}</button></div>
    </section>
    {customRootIssue && !busy && <CustomSourceRootDialog message={customRootIssue.message} initialRoot={customRootIssue.attemptedRoot ?? ""} onClose={() => setCustomRootIssue(null)} onRetry={(sourceRoot) => { void submit(sourceRoot); }} />}
  </main>;
}
