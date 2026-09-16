import { PanelRightClose, PanelRightOpen, UsersRound } from "lucide-react";
import type { ConversationSummary, MessageItem, ParticipantItem } from "../domain/types";
import { useState } from "react";
import { ParticipantCard } from "./ParticipantCard";

interface DetailPanelProps {
  conversation: ConversationSummary;
  participants: ParticipantItem[];
  messages: MessageItem[];
  collapsed: boolean;
  onToggleCollapsed: () => void;
}

export function DetailPanel({ conversation, participants, messages, collapsed, onToggleCollapsed }: DetailPanelProps) {
  const [selectedParticipant, setSelectedParticipant] = useState<ParticipantItem>();
  const { announcement, board } = extractGroupInfo(messages);
  const hasGroupInfo = Boolean(announcement || board);
  return (
    <aside className={collapsed ? "detail-pane collapsed" : "detail-pane"} aria-label="会话详情与导出">
      <h2><span>会话详情</span><button className="detail-collapse-button" type="button" aria-label={collapsed ? "展开会话详情" : "收起会话详情"} title={collapsed ? "展开会话详情" : "收起会话详情"} onClick={onToggleCollapsed}>{collapsed ? <PanelRightOpen size={17} /> : <PanelRightClose size={17} />}</button></h2>
      {!collapsed && <>
      <section className="detail-card">
        {announcement && <section className="group-board-card"><h3>群公告 <span>›</span></h3><p>{announcement}</p></section>}
        {board && <section className="group-board-card"><h3>群看板 <span>›</span></h3><p>{board}</p></section>}
        {hasGroupInfo && <div className="detail-separator" />}
        <div className="detail-link"><span><UsersRound size={16} />群成员</span><strong>{conversation.participantCount} 人</strong></div>
        <div className="participant-strip">{participants.slice(0, 12).map((participant) => <button key={participant.id} type="button" title={participant.name} onClick={() => setSelectedParticipant(participant)}><span>{Array.from(participant.name).at(-1) || "员"}</span><small>{participant.name}</small></button>)}</div>
      </section>
      </>}
      {selectedParticipant && <ParticipantCard participant={selectedParticipant} onClose={() => setSelectedParticipant(undefined)} />}
    </aside>
  );
}

export function extractGroupInfo(messages: MessageItem[]): { announcement?: string; board?: string } {
  const systemMessages = messages
    .filter((message) => message.direction === "system")
    .map((message) => ({ rawType: message.rawType, body: normalizeGroupInfoText(message.body) }))
    .filter((message): message is { rawType: string | undefined; body: string } => Boolean(message.body));
  const boardMessage = systemMessages.find((message) => message.rawType === "group_board" || /群看板|工作看板|工作进展/.test(message.body));
  const announcementMessage = systemMessages.find((message) =>
    message !== boardMessage && (message.rawType === "group_announcement" || /群公告|(?:公告|通知)(?:内容)?[：:]?/.test(message.body) || (message.body.includes("\n") && /各位|注意事项|请大家|请及时/.test(message.body))),
  );
  return {
    announcement: cleanGroupInfoLabel(announcementMessage?.body, "群公告"),
    board: cleanGroupInfoLabel(boardMessage?.body, "群看板"),
  };
}

function normalizeGroupInfoText(value?: string): string | undefined {
  if (!value?.trim()) return undefined;
  const lines = value
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line && !/^\d{8,}$/.test(line) && !/^[0-9a-f]{24,}$/i.test(line));
  const text = lines.join("\n").trim();
  if (!text || /^\d+$/.test(text)) return undefined;
  return text;
}

function cleanGroupInfoLabel(value: string | undefined, label: "群公告" | "群看板"): string | undefined {
  if (!value) return undefined;
  const prefix = label === "群公告" ? /^(?:群公告|公告)(?:内容)?[：:]?\s*/u : /^(?:群看板|工作看板)(?:内容)?[：:]?\s*/u;
  return value.replace(prefix, "").trim() || undefined;
}
