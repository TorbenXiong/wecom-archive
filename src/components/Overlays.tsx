import { AlertTriangle, CheckCircle2, Database, FileJson2, LockKeyhole, ShieldCheck, UploadCloud, X } from "lucide-react";
import { useState } from "react";
import type { ExportFormat, ExportScope } from "../domain/types";
import { importClientJson } from "../lib/server-api";

export function ExportDialog({ scope, format, messageCount, mediaCount, onClose, onConfirm }: { scope: ExportScope; format: ExportFormat; messageCount: number; mediaCount: number; onClose: () => void; onConfirm: () => Promise<void> }) {
  const all = scope === "entire_archive";
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const submit = async () => {
    setBusy(true);
    setError(undefined);
    try {
      await onConfirm();
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "服务端导出失败。" );
      setBusy(false);
    }
  };
  return <div className="modal-backdrop" role="presentation" onMouseDown={busy ? undefined : onClose}><section className="modal-card" role="dialog" aria-modal="true" aria-labelledby="export-title" onMouseDown={(event) => event.stopPropagation()}><button className="modal-close" type="button" onClick={onClose} disabled={busy}><X size={18} /></button><span className={all ? "modal-icon warning" : "modal-icon"}>{all ? <AlertTriangle /> : <CheckCircle2 />}</span><h2 id="export-title">{all ? "确认导出全部档案" : "准备导出"}</h2><p>{all ? "此操作将包含全部会话和关联媒体。请确认服务端导出目录具备足够空间。" : "导出由服务端受控任务完成，成功前仅创建 .partial 临时文件。"}</p><dl className="estimate-grid"><div><dt>预计消息</dt><dd>{messageCount.toLocaleString("zh-CN")}</dd></div><div><dt>媒体项目</dt><dd>{mediaCount.toLocaleString("zh-CN")}</dd></div><div><dt>输出</dt><dd>ZIP 包</dd></div><div><dt>格式</dt><dd>{format.toUpperCase()}</dd></div></dl>{error && <div className="inline-error"><AlertTriangle size={15} />{error}</div>}<div className="modal-actions"><button className="secondary-button" type="button" onClick={onClose} disabled={busy}>取消</button><button className="primary-button" type="button" onClick={submit} disabled={busy}>{busy ? "正在生成…" : "创建导出任务"}</button></div></section></div>;
}

export function ServerAccessGate({ onConnect }: { onConnect: (token: string) => Promise<void> }) {
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (token.length < 24 || busy) return;
    setBusy(true);
    setError(undefined);
    try {
      await onConnect(token);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "无法连接归档服务。请检查访问令牌。" );
    } finally {
      setBusy(false);
    }
  };

  return <div className="access-shell"><header className="client-header"><div className="brand-lockup"><span className="brand-mark"><Database size={18} /></span><span className="brand-name">企微归档服务端</span><span className="mode-badge">中央归档</span></div><span className="privacy-note"><ShieldCheck size={15} />默认仅限本机访问</span></header><main className="access-main"><form className="access-card" onSubmit={submit}><span className="access-icon"><LockKeyhole size={28} /></span><h1>连接中央归档</h1><p>输入启动服务时配置的访问令牌。令牌仅保存在当前页面内存中，刷新或关闭页面后立即清除。</p><label className="access-token"><span>服务端访问令牌</span><div><LockKeyhole size={16} /><input aria-label="服务端访问令牌" type="password" autoComplete="off" value={token} onChange={(event) => setToken(event.target.value)} placeholder="至少 24 个字符" autoFocus /></div></label>{error && <div className="inline-error"><AlertTriangle size={15} />{error}</div>}<button className="primary-button" disabled={token.length < 24 || busy} type="submit">{busy ? "正在验证…" : "验证并进入工作台"}</button><small>服务端不会将令牌写入浏览器存储。</small></form></main></div>;
}

export function ServerImportDialog({ token, onClose, onImported }: { token: string; onClose: () => void; onImported: (message: string) => void }) {
  const [file, setFile] = useState<File>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();

  const submit = async () => {
    if (!file || !token) return;
    setBusy(true);
    setError(undefined);
    try {
      const result = await importClientJson(file, token);
      onImported(`JSON 导入完成：新增 ${result.inserted} 条，修订 ${result.revised} 条，已存在 ${result.unchanged} 条。`);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "客户端 JSON 导入失败。");
    } finally {
      setBusy(false);
    }
  };

  return <div className="modal-backdrop" role="presentation" onMouseDown={onClose}><section className="modal-card import-dialog" role="dialog" aria-modal="true" aria-labelledby="import-title" onMouseDown={(event) => event.stopPropagation()}><button className="modal-close" type="button" onClick={onClose}><X size={18} /></button><span className="modal-icon"><UploadCloud /></span><h2 id="import-title">导入客户端 JSON</h2><p>服务端会先验证 <code>client-export.v1</code>、消息与批次关系以及 SHA-256，再执行幂等归档。</p><label className="file-drop"><FileJson2 size={26} /><span><strong>{file?.name ?? "选择客户端 JSON 文件"}</strong><small>{file ? `${(file.size / 1024 / 1024).toFixed(2)} MB` : "仅接受由采集客户端生成的 .json"}</small></span><input type="file" accept="application/json,.json" onChange={(event) => setFile(event.target.files?.[0])} /></label>{error && <div className="inline-error"><AlertTriangle size={15} />{error}</div>}<div className="modal-actions"><button className="secondary-button" type="button" onClick={onClose}>取消</button><button className="primary-button" disabled={!file || busy} type="button" onClick={submit}>{busy ? "正在校验并导入…" : "校验并导入"}</button></div></section></div>;
}

export function ServerSettingsPanel({ onClose, onImport, onDisconnect }: { onClose: () => void; onImport: () => void; onDisconnect: () => void }) {
  return <div className="modal-backdrop settings-backdrop" role="presentation" onMouseDown={onClose}><section className="settings-panel" role="dialog" aria-modal="true" aria-labelledby="server-settings-title" onMouseDown={(event) => event.stopPropagation()}><header><div><h2 id="server-settings-title">服务端设置</h2><p>中央归档、安全与保留</p></div><button className="icon-button" onClick={onClose} type="button"><X size={19} /></button></header><div className="settings-content"><section><h3>归档存储</h3><div className="setting-action"><span className="setting-icon"><Database size={19} /></span><span><strong>单节点归档库</strong><small>FTS5 检索、幂等批次、消息修订与不可变审计事件</small></span></div></section><section><h3>客户端导入</h3><div className="setting-action"><span className="setting-icon"><FileJson2 size={19} /></span><span><strong>ClientExportV1</strong><small>校验 schema、批次关系、消息 ID、计数和 SHA-256</small></span><button className="secondary-button compact" type="button" onClick={onImport}>导入 JSON</button></div></section><section><h3>网络边界</h3><div className="setting-action"><span className="setting-icon"><ShieldCheck size={19} /></span><span><strong>默认仅监听 127.0.0.1</strong><small>访问令牌只保存在当前页面内存；对内网开放前必须配置 TLS 反向代理和组织身份认证</small></span><button className="secondary-button compact" type="button" onClick={onDisconnect}>断开</button></div></section><section><h3>关于与开源许可</h3><div className="license-copy"><strong>企业微信记录归档服务端 0.1.0</strong><p>Copyright (c) 2026 Torben Xiong · MIT License</p><p>服务端负责导入、浏览、检索、审计、保留策略和富格式导出。</p></div></section></div></section></div>;
}

export function Toast({ children, onClose, actionLabel, onAction }: { children: React.ReactNode; onClose: () => void; actionLabel?: string; onAction?: () => void }) {
  return <div className="toast" role="status"><CheckCircle2 size={18} /><span>{children}</span>{actionLabel && onAction && <button className="toast-action" type="button" onClick={onAction}>{actionLabel}</button>}<button className="toast-close" type="button" aria-label="关闭提示" onClick={onClose}><X size={15} /></button></div>;
}

export function InfoDialog({ title, children, onConfirm, actionLabel, onAction, tone = "success" }: { title: string; children: React.ReactNode; onConfirm: () => void; actionLabel?: string; onAction?: () => void; tone?: "success" | "error" }) {
  return <div className="modal-backdrop client-info-backdrop" role="presentation"><section className={`modal-card client-info-dialog ${tone}`} role="dialog" aria-modal="true" aria-labelledby="client-info-title"><span className="modal-icon">{tone === "success" ? <CheckCircle2 /> : <AlertTriangle />}</span><h2 id="client-info-title">{title}</h2><p>{children}</p><div className="modal-actions">{actionLabel && onAction && <button className="secondary-button" type="button" onClick={onAction}>{actionLabel}</button>}<button className="primary-button" type="button" onClick={onConfirm}>确认</button></div></section></div>;
}
