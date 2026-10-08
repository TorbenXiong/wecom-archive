import { AlertTriangle, CheckCircle2, Database, LoaderCircle, LockKeyhole, ShieldCheck, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import type { ExportFormat, ExportScope } from "../domain/types";
import type { ConversationSummary } from "../domain/types";
import type { LocalCollectionProgress } from "../lib/server-api";
import type { ExportOrder } from "../lib/server-api";

import { ExportDirectoryButton } from "./ExportDirectoryButton";
import { ExportPresentationOptions } from "./ExportPresentationOptions";
import { ExportOrderOptions } from "./ExportOrderOptions";

export interface ExportSelection {
  conversationIds: string[];
  startDate?: string;
  endDate?: string;
}

export function ExportDialog({ token, scope, format, dataRedaction, simplify, pretty, conversationOrder, messageOrder, onConversationOrderChange, onMessageOrderChange, onSimplifyChange, onPrettyChange, messageCount, mediaCount, conversations, selectedConversationId, onScopeChange, onFormatChange, onDataRedactionChange, onOpenDirectoryFailed, onClose, onConfirm }: { token: string; scope: ExportScope; format: ExportFormat; dataRedaction: boolean; simplify: boolean; pretty: boolean; conversationOrder: ExportOrder; messageOrder: ExportOrder; onConversationOrderChange: (order: ExportOrder) => void; onMessageOrderChange: (order: ExportOrder) => void; onSimplifyChange: (enabled: boolean) => void; onPrettyChange: (enabled: boolean) => void; messageCount: number; mediaCount: number; conversations: ConversationSummary[]; selectedConversationId: string; onScopeChange: (scope: ExportScope) => void; onFormatChange: (format: ExportFormat) => void; onDataRedactionChange: (enabled: boolean) => void; onOpenDirectoryFailed: (error: unknown) => void; onClose: () => void; onConfirm: (selection: ExportSelection) => Promise<void> }) {
  const all = scope === "entire_archive";
  const [selectedIds, setSelectedIds] = useState<string[]>(selectedConversationId ? [selectedConversationId] : []);
  const [startDate, setStartDate] = useState("");
  const [endDate, setEndDate] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string>();
  const submit = async () => {
    setBusy(true);
    setError(undefined);
    try {
      if (scope === "selected_conversations" && selectedIds.length === 0) throw new Error("请至少选择一个会话。");
      if (scope === "date_range" && !startDate && !endDate) throw new Error("请至少填写开始日期或结束日期。");
      await onConfirm({ conversationIds: selectedIds, startDate: startDate || undefined, endDate: endDate || undefined });
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "服务端导出失败。" );
      setBusy(false);
    }
  };
  const toggleConversation = (id: string) => setSelectedIds((current) => current.includes(id) ? current.filter((item) => item !== id) : [...current, id]);
  const estimatedMessages = scope === "selected_conversations" ? "按所选会话" : scope === "date_range" ? "按日期筛选" : messageCount.toLocaleString("zh-CN");
  const estimatedMedia = scope === "selected_conversations" || scope === "date_range" ? "按筛选结果" : mediaCount.toLocaleString("zh-CN");
  return <div className="modal-backdrop" role="presentation" onMouseDown={busy ? undefined : onClose}><section className="modal-card export-dialog-card" role="dialog" aria-modal="true" aria-labelledby="export-title" onMouseDown={(event) => event.stopPropagation()}><div className="export-dialog-corner-actions"><ExportDirectoryButton token={token} onFailed={onOpenDirectoryFailed} /><button className="modal-close" type="button" aria-label="关闭导出弹窗" onClick={onClose} disabled={busy}><X size={18} /></button></div><span className={all ? "modal-icon warning" : "modal-icon"}>{all ? <AlertTriangle /> : <CheckCircle2 />}</span><h2 id="export-title">{all ? "确认导出全部档案" : "准备导出"}</h2><p>{all ? "此操作将包含全部会话记录。请确认服务端导出目录具备足够空间。" : "选择导出范围、排序和格式后开始生成。"}</p><div className="export-dialog-options"><fieldset><legend>导出范围</legend>{([ ["current_conversation", "当前会话"], ["current_filter", "当前筛选"], ["selected_conversations", "按会话（多选）"], ["date_range", "按日期"], ["entire_archive", "全部档案"] ] as const).map(([value, label]) => <label key={value}><input type="radio" name="export-scope" checked={scope === value} onChange={() => onScopeChange(value)} />{label}</label>)}</fieldset>{scope === "selected_conversations" && <fieldset className="export-conversation-picker"><legend>选择会话（已选 {selectedIds.length} 个）</legend><div>{conversations.map((conversation) => <label key={conversation.id}><input type="checkbox" checked={selectedIds.includes(conversation.id)} onChange={() => toggleConversation(conversation.id)} />{conversation.title}<small>{conversation.messageCount.toLocaleString("zh-CN")} 条</small></label>)}</div></fieldset>}{scope === "date_range" && <fieldset className="export-date-range"><legend>日期范围</legend><label>开始日期<input type="date" value={startDate} onChange={(event) => setStartDate(event.target.value)} /></label><label>结束日期<input type="date" value={endDate} onChange={(event) => setEndDate(event.target.value)} /></label></fieldset>}<ExportOrderOptions conversationOrder={conversationOrder} messageOrder={messageOrder} onConversationOrderChange={onConversationOrderChange} onMessageOrderChange={onMessageOrderChange} disabled={busy} /><fieldset><legend>导出格式</legend><div className="format-grid">{(["json", "csv", "html", "md"] as ExportFormat[]).map((item) => <button className={format === item ? "selected" : ""} key={item} type="button" onClick={() => onFormatChange(item)}>{item.toUpperCase()}</button>)}</div></fieldset><ExportPresentationOptions dataRedaction={dataRedaction} simplify={simplify} pretty={pretty} onDataRedactionChange={onDataRedactionChange} onSimplifyChange={onSimplifyChange} onPrettyChange={onPrettyChange} disabled={busy} /></div><dl className="estimate-grid"><div><dt>预计消息</dt><dd>{estimatedMessages}</dd></div><div><dt>媒体项目</dt><dd>{estimatedMedia}</dd></div><div><dt>输出</dt><dd>单个文件</dd></div><div><dt>格式</dt><dd>{format.toUpperCase()}</dd></div></dl>{error && <div className="inline-error"><AlertTriangle size={15} />{error}</div>}<div className="modal-actions"><button className="secondary-button" type="button" onClick={onClose} disabled={busy}>取消</button><button className="primary-button" type="button" onClick={submit} disabled={busy}>{busy ? "正在生成…" : "开始导出"}</button></div></section></div>;
}

export function LocalCollectionDialog({ progress }: { progress: LocalCollectionProgress }) {
  const percent = Math.max(0, Math.min(100, Math.round(progress.percent)));
  return <div className="modal-backdrop local-collection-backdrop" role="presentation"><section className="modal-card local-collection-dialog" role="dialog" aria-modal="true" aria-labelledby="local-collection-title"><span className="modal-icon collecting"><LoaderCircle className="spin" /></span><h2 id="local-collection-title">正在采集本机数据</h2><strong className="collection-stage">{progress.stage}</strong><p>{progress.detail}</p><div className="collection-progress" role="progressbar" aria-label="本机采集进度" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}><span style={{ width: `${percent}%` }} /></div><div className="collection-progress-meta"><span>{percent}%</span><small>请保持企业微信登录，采集期间无需进行其他操作。</small></div></section></div>;
}

export function LocalCollectionSetupDialog({ onClose, onCollect }: { onClose: () => void; onCollect: (includeMedia: boolean) => void }) {
  return <div className="modal-backdrop" role="presentation" onMouseDown={onClose}><section className="modal-card local-collection-setup" role="dialog" aria-modal="true" aria-labelledby="local-collection-setup-title" onMouseDown={(event) => event.stopPropagation()}><button className="modal-close" type="button" aria-label="关闭本机采集配置" onClick={onClose}><X size={18} /></button><span className="modal-icon"><Database /></span><h2 id="local-collection-setup-title">本机采集</h2><p>选择本次采集范围。持续采集请前往“采集端管理”维护。</p><div className="local-collection-choices"><button className="secondary-button" type="button" onClick={() => onCollect(false)}>仅采集文本</button><button className="primary-button" type="button" onClick={() => onCollect(true)}>包含全内容</button></div></section></div>;
}

export function CustomSourceRootDialog({ message, initialRoot, onClose, onRetry }: { message: string; initialRoot: string; onClose: () => void; onRetry: (sourceRoot: string) => void }) {
  const [sourceRoot, setSourceRoot] = useState(initialRoot);
  const trimmed = sourceRoot.trim();
  return <div className="modal-backdrop" role="presentation" onMouseDown={onClose}><section className="modal-card local-collection-setup" role="dialog" aria-modal="true" aria-labelledby="custom-source-root-title" onMouseDown={(event) => event.stopPropagation()}><button className="modal-close" type="button" aria-label="关闭自定义数据目录" onClick={onClose}><X size={18} /></button><span className="modal-icon warning"><AlertTriangle /></span><h2 id="custom-source-root-title">没有找到企业微信数据</h2><div className="inline-error"><AlertTriangle size={15} />{message}</div><p>如果企业微信把聊天记录存放在别的盘，或“文档”被 OneDrive 接管，请把企业微信设置中的存储位置填进来重试。成功后这台电脑会记住，下次不用再填。</p><label className="access-token"><span>企业微信数据目录</span><div><input aria-label="企业微信数据目录" type="text" autoComplete="off" spellCheck={false} value={sourceRoot} onChange={(event) => setSourceRoot(event.target.value)} placeholder="例如 D:\\WXWork" autoFocus /></div></label><div className="modal-actions"><button className="secondary-button" type="button" onClick={onClose}>取消</button><button className="primary-button" type="button" disabled={trimmed.length === 0} onClick={() => onRetry(trimmed)}>用这个目录重试</button></div></section></div>;
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

  return <div className="access-shell"><header className="client-header"><div className="brand-lockup"><span className="brand-mark"><Database size={18} /></span><span className="brand-name">企微归档</span><span className="mode-badge">工作台</span></div><span className="privacy-note"><ShieldCheck size={15} />默认仅限本机访问</span></header><main className="access-main"><form className="access-card" onSubmit={submit}><span className="access-icon"><LockKeyhole size={28} /></span><h1>连接归档工作台</h1><p>输入启动服务时配置的访问令牌。令牌仅保存在当前页面内存中，刷新或关闭页面后立即清除。</p><label className="access-token"><span>服务端访问令牌</span><div><LockKeyhole size={16} /><input aria-label="服务端访问令牌" type="password" autoComplete="off" value={token} onChange={(event) => setToken(event.target.value)} placeholder="至少 24 个字符" autoFocus /></div></label>{error && <div className="inline-error"><AlertTriangle size={15} />{error}</div>}<button className="primary-button" disabled={token.length < 24 || busy} type="submit">{busy ? "正在验证…" : "验证并进入工作台"}</button><small>服务端不会将令牌写入浏览器存储。</small></form></main></div>;
}

export function Toast({ children, onClose, actionLabel, onAction, tone = "success", duration = 4000 }: { children: React.ReactNode; onClose: () => void; actionLabel?: string; onAction?: () => void; tone?: "success" | "error"; duration?: number }) {
  const onCloseRef = useRef(onClose);
  useEffect(() => { onCloseRef.current = onClose; }, [onClose]);
  useEffect(() => {
    const timer = window.setTimeout(() => onCloseRef.current(), duration);
    return () => window.clearTimeout(timer);
  }, [duration]);
  return <div className={`toast ${tone}`} role={tone === "error" ? "alert" : "status"}>{tone === "success" ? <CheckCircle2 size={18} /> : <AlertTriangle size={18} />}<span>{children}</span>{actionLabel && onAction && <button className="toast-action" type="button" onClick={onAction}>{actionLabel}</button>}<button className="toast-close" type="button" aria-label="关闭提示" onClick={onClose}><X size={15} /></button></div>;
}

export function InfoDialog({ title, children, onConfirm, actionLabel, onAction, tone = "success" }: { title: string; children: React.ReactNode; onConfirm: () => void; actionLabel?: string; onAction?: () => void; tone?: "success" | "error" }) {
  return <div className="modal-backdrop client-info-backdrop" role="presentation"><section className={`modal-card client-info-dialog ${tone}`} role="dialog" aria-modal="true" aria-labelledby="client-info-title"><span className="modal-icon">{tone === "success" ? <CheckCircle2 /> : <AlertTriangle />}</span><h2 id="client-info-title">{title}</h2><p>{children}</p><div className="modal-actions">{actionLabel && onAction && <button className="secondary-button" type="button" onClick={onAction}>{actionLabel}</button>}<button className="primary-button" type="button" onClick={onConfirm}>确认</button></div></section></div>;
}
