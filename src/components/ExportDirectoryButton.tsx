import { FolderOpen, LoaderCircle } from "lucide-react";
import { useState } from "react";
import { openExportDirectory } from "../lib/server-api";

export function ExportDirectoryButton({ token, onFailed }: { token: string; onFailed: (error: unknown) => void }) {
  const [opening, setOpening] = useState(false);
  const open = async () => {
    if (opening) return;
    setOpening(true);
    try {
      await openExportDirectory(token);
    } catch (error) {
      onFailed(error);
    } finally {
      setOpening(false);
    }
  };
  return <button className="export-directory-button" type="button" disabled={opening} onClick={() => void open()} title="打开导出目录">{opening ? <LoaderCircle className="spin" size={15} /> : <FolderOpen size={15} />}导出目录</button>;
}
