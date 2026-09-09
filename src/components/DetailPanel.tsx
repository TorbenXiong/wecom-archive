import { CheckCircle2, ChevronRight, CircleHelp, FileArchive, UsersRound } from "lucide-react";
import type { ConversationSummary, ExportFormat, ExportScope } from "../domain/types";

interface DetailPanelProps {
  conversation: ConversationSummary;
  scope: ExportScope;
  format: ExportFormat;
  onScopeChange: (scope: ExportScope) => void;
  onFormatChange: (format: ExportFormat) => void;
  onExport: () => void;
}

export function DetailPanel({ conversation, scope, format, onScopeChange, onFormatChange, onExport }: DetailPanelProps) {
  return (
    <aside className="detail-pane" aria-label="会话详情与导出">
      <h2>会话详情</h2>
      <section className="detail-card">
        <button className="detail-link" type="button"><span><UsersRound size={16} />参与人</span><strong>{conversation.participantCount} 人</strong><ChevronRight size={16} /></button>
        <div className="participant-strip">{Array.from({ length: Math.min(conversation.participantCount, 6) }, (_, index) => <span key={index}>{String.fromCharCode(65 + index)}</span>)}</div>
        <div className="detail-separator" />
        <button className="detail-link" type="button"><span>时间范围</span><ChevronRight size={16} /></button>
        <p className="date-range">2026-09-01 00:00:00<br />2026-09-07 23:59:59</p>
        <div className="detail-separator" />
        <button className="detail-link" type="button"><span>消息类型</span><ChevronRight size={16} /></button>
        <dl className="type-stats"><div><dt>文本</dt><dd>182 (76.5%)</dd></div><div><dt>图片</dt><dd>32 (13.4%)</dd></div><div><dt>文件</dt><dd>18 (7.6%)</dd></div><div><dt>系统</dt><dd>6 (2.5%)</dd></div></dl>
        <div className="detail-separator" />
        <h3>媒体完整性 <CircleHelp size={14} /></h3>
        <div className="integrity"><div className="integrity-ring"><strong>100%</strong></div><div><strong>{conversation.mediaCount} / {conversation.mediaCount} 项</strong><span><CheckCircle2 size={13} />所有媒体文件已完整</span><small>本地可用</small></div></div>
      </section>
      <section className="export-card">
        <h3><FileArchive size={16} />导出范围</h3>
        <ScopeRadio checked={scope === "current_conversation"} label="当前会话" detail={`预计 ${conversation.messageCount} 条消息 · ${conversation.mediaCount} 项媒体`} onChange={() => onScopeChange("current_conversation")} />
        <ScopeRadio checked={scope === "current_filter"} label="当前筛选" onChange={() => onScopeChange("current_filter")} />
        <ScopeRadio checked={scope === "entire_archive"} label="全部档案" onChange={() => onScopeChange("entire_archive")} />
        <button className="primary-button export-button" onClick={onExport} type="button">开始导出</button>
        <h3 className="format-heading">导出格式</h3>
        <div className="format-grid">{(["json", "csv", "html", "pdf"] as ExportFormat[]).map((item) => <button className={format === item ? "selected" : ""} key={item} type="button" onClick={() => onFormatChange(item)}>{item.toUpperCase()}</button>)}</div>
      </section>
    </aside>
  );
}

function ScopeRadio({ checked, label, detail, onChange }: { checked: boolean; label: string; detail?: string; onChange: () => void }) {
  return <label className="scope-radio"><input type="radio" checked={checked} onChange={onChange} /><span className="radio-mark" /><span><strong>{label}</strong>{detail && <small>{detail}</small>}</span></label>;
}
