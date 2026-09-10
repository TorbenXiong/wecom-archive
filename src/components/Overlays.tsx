import { AlertTriangle, CheckCircle2, Database, LockKeyhole, ShieldCheck, X } from "lucide-react";
import { useState } from "react";
import type { ExportFormat, ExportScope } from "../domain/types";

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

  return <div className="access-shell"><header className="client-header"><div className="brand-lockup"><span className="brand-mark"><Database size={18} /></span><span className="brand-name">企微归档</span><span className="mode-badge">企业版</span></div><span className="privacy-note"><ShieldCheck size={15} />默认仅限本机访问</span></header><main className="access-main"><form className="access-card" onSubmit={submit}><span className="access-icon"><LockKeyhole size={28} /></span><h1>连接企业版归档</h1><p>输入启动服务时配置的访问令牌。令牌仅保存在当前页面内存中，刷新或关闭页面后立即清除。</p><label className="access-token"><span>服务端访问令牌</span><div><LockKeyhole size={16} /><input aria-label="服务端访问令牌" type="password" autoComplete="off" value={token} onChange={(event) => setToken(event.target.value)} placeholder="至少 24 个字符" autoFocus /></div></label>{error && <div className="inline-error"><AlertTriangle size={15} />{error}</div>}<button className="primary-button" disabled={token.length < 24 || busy} type="submit">{busy ? "正在验证…" : "验证并进入工作台"}</button><small>服务端不会将令牌写入浏览器存储。</small></form></main></div>;
}

export function ServerSettingsPanel({ onClose, onDisconnect }: { onClose: () => void; onDisconnect: () => void }) {
  return <div className="modal-backdrop settings-backdrop" role="presentation" onMouseDown={onClose}><section className="settings-panel" role="dialog" aria-modal="true" aria-labelledby="server-settings-title" onMouseDown={(event) => event.stopPropagation()}><header><div><h2 id="server-settings-title">企业版设置</h2><p>中央归档、安全与保留</p></div><button className="icon-button" onClick={onClose} type="button"><X size={19} /></button></header><div className="settings-content"><section><h3>归档存储</h3><div className="setting-action"><span className="setting-icon"><Database size={19} /></span><span><strong>单节点归档库</strong><small>FTS5 检索、幂等批次、消息修订与不可变审计记录</small></span></div></section><section><h3>加密收集 · 规划中</h3><div className="license-copy"><strong>配置加密信息 → 生成员工收集端 → 导入解密</strong><p>企业版将根据加密配置生成专属采集端。员工授权后离线采集并输出密文，企业版使用对应配置解密。</p><p>企业版只接收专属采集端生成的加密数据。当前版本尚未开放配置、生成或加密导入。</p></div></section><section><h3>网络边界</h3><div className="setting-action"><span className="setting-icon"><ShieldCheck size={19} /></span><span><strong>默认仅监听 127.0.0.1</strong><small>访问令牌只保存在当前页面内存中；对内网开放前必须配置 TLS、身份认证和最小权限网络策略。</small></span><button className="secondary-button compact" type="button" onClick={onDisconnect}>断开</button></div></section><section><h3>关于与开源许可</h3><div className="license-copy"><strong>企业微信记录归档 · 企业版 0.0.1</strong><p>Copyright (c) 2026 Torben Xiong · MIT License</p><p>企业版负责浏览、检索、审计、保留策略和富格式导出。</p></div></section></div></section></div>;
}

export function Toast({ children, onClose, actionLabel, onAction }: { children: React.ReactNode; onClose: () => void; actionLabel?: string; onAction?: () => void }) {
  return <div className="toast" role="status"><CheckCircle2 size={18} /><span>{children}</span>{actionLabel && onAction && <button className="toast-action" type="button" onClick={onAction}>{actionLabel}</button>}<button className="toast-close" type="button" aria-label="关闭提示" onClick={onClose}><X size={15} /></button></div>;
}

export function InfoDialog({ title, children, onConfirm, actionLabel, onAction, tone = "success" }: { title: string; children: React.ReactNode; onConfirm: () => void; actionLabel?: string; onAction?: () => void; tone?: "success" | "error" }) {
  return <div className="modal-backdrop client-info-backdrop" role="presentation"><section className={`modal-card client-info-dialog ${tone}`} role="dialog" aria-modal="true" aria-labelledby="client-info-title"><span className="modal-icon">{tone === "success" ? <CheckCircle2 /> : <AlertTriangle />}</span><h2 id="client-info-title">{title}</h2><p>{children}</p><div className="modal-actions">{actionLabel && onAction && <button className="secondary-button" type="button" onClick={onAction}>{actionLabel}</button>}<button className="primary-button" type="button" onClick={onConfirm}>确认</button></div></section></div>;
}
