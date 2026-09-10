import { useDeferredValue, useEffect, useMemo, useState } from "react";
import { AppHeader } from "./components/AppHeader";
import { ConversationList } from "./components/ConversationList";
import { DetailPanel } from "./components/DetailPanel";
import { MessageTimeline } from "./components/MessageTimeline";
import { Navigation } from "./components/Navigation";
import { ExportDialog, ServerAccessGate, ServerSettingsPanel, Toast } from "./components/Overlays";
import type { ConversationSummary, ExportFormat, ExportScope, FilterState, MessageItem } from "./domain/types";
import { createServerExport, getArchiveSummary, listConversations, listMessages, type ServerArchiveSummary, type ServerConversation, type ServerMessage } from "./lib/server-api";
import "./styles.css";

type Section = "conversations" | "exports" | "settings";

const defaultFilters: FilterState = { search: "", date: "all", participant: "all", type: "all", mediaOnly: false };
const emptySummary: ServerArchiveSummary = { conversation_count: 0, message_count: 0, media_count: 0 };
const emptyConversation: ConversationSummary = {
  id: "",
  title: "尚无归档数据",
  initials: "档",
  accent: "blue",
  lastMessage: "等待企业版专属采集端数据",
  lastAt: "--",
  unread: 0,
  messageCount: 0,
  mediaCount: 0,
  participantCount: 0,
};

export default function App() {
  const [token, setToken] = useState<string>();
  const [summary, setSummary] = useState(emptySummary);
  const [conversations, setConversations] = useState<ConversationSummary[]>([]);
  const [messages, setMessages] = useState<MessageItem[]>([]);
  const [section, setSection] = useState<Section>("conversations");
  const [selectedId, setSelectedId] = useState("");
  const [filters, setFilters] = useState(defaultFilters);
  const [scope, setScope] = useState<ExportScope>("current_conversation");
  const [format, setFormat] = useState<ExportFormat>("json");
  const [showExport, setShowExport] = useState(false);
  const [messageStatus, setMessageStatus] = useState("等待选择会话");
  const [toast, setToast] = useState<string>();
  const deferredSearch = useDeferredValue(filters.search.trim());

  const loadDirectory = async (accessToken: string) => {
    const [nextSummary, rows] = await Promise.all([
      getArchiveSummary(accessToken),
      listConversations(accessToken),
    ]);
    const nextConversations = rows.map(mapConversation);
    setSummary(nextSummary);
    setConversations(nextConversations);
    setSelectedId((current) => nextConversations.some((item) => item.id === current) ? current : (nextConversations[0]?.id ?? ""));
  };

  const connect = async (accessToken: string) => {
    await loadDirectory(accessToken);
    setToken(accessToken);
  };

  useEffect(() => {
    if (!token || !selectedId) {
      setMessages([]);
      return;
    }
    const controller = new AbortController();
    setMessageStatus("正在读取服务端归档…");
    listMessages(token, {
      conversationId: selectedId,
      participantId: filters.participant === "all" ? undefined : filters.participant,
      text: deferredSearch || undefined,
      messageType: filters.type === "all" ? undefined : filters.type,
      mediaOnly: filters.mediaOnly,
      limit: 500,
    }, controller.signal).then((rows) => {
      setMessages(rows.map(mapMessage));
      setMessageStatus(rows.length === 0 ? "当前条件下没有消息" : "");
    }).catch((reason) => {
      if (reason instanceof DOMException && reason.name === "AbortError") return;
      setMessages([]);
      setMessageStatus(reason instanceof Error ? reason.message : "消息读取失败");
    });
    return () => controller.abort();
  }, [deferredSearch, filters.mediaOnly, filters.participant, filters.type, selectedId, token]);

  const selectedConversation = conversations.find((item) => item.id === selectedId) ?? emptyConversation;
  const participantOptions = useMemo(() => {
    const values = new Map<string, string>();
    for (const message of messages) {
      if (message.senderId !== "system") values.set(message.senderId, message.senderName);
    }
    return [...values].map(([id, label]) => ({ id, label }));
  }, [messages]);

  if (!token) return <ServerAccessGate onConnect={connect} />;

  return <div className="app-shell">
    <AppHeader />
    <div className="workspace-grid">
      <Navigation active={section} onSelect={setSection} />
      <ConversationList conversations={conversations} selectedId={selectedId} filters={filters} participantOptions={participantOptions} onFiltersChange={setFilters} onSelect={setSelectedId} />
      <MessageTimeline conversation={selectedConversation} messages={messages} status={messageStatus} />
      <DetailPanel conversation={selectedConversation} scope={scope} format={format} onScopeChange={setScope} onFormatChange={setFormat} onExport={() => setShowExport(true)} />
    </div>
    <footer className="status-bar"><span>已归档 {summary.message_count.toLocaleString("zh-CN")} 条消息 · {summary.media_count.toLocaleString("zh-CN")} 项媒体 · {summary.conversation_count.toLocaleString("zh-CN")} 个会话</span><span><i />受控访问 · 审计已启用</span></footer>
    {section === "settings" && <ServerSettingsPanel onClose={() => setSection("conversations")} onDisconnect={() => { setToken(undefined); setMessages([]); setConversations([]); setSummary(emptySummary); setSection("conversations"); }} />}
    {showExport && <ExportDialog scope={scope} format={format} messageCount={scope === "entire_archive" ? summary.message_count : scope === "current_filter" ? messages.length : selectedConversation.messageCount} mediaCount={scope === "entire_archive" ? summary.media_count : scope === "current_filter" ? messages.filter((message) => message.attachment).length : selectedConversation.mediaCount} onClose={() => setShowExport(false)} onConfirm={async () => { const result = await createServerExport(token, { scope, format, conversationId: selectedId || undefined, participantId: filters.participant === "all" ? undefined : filters.participant, text: filters.search.trim() || undefined, messageType: filters.type === "all" ? undefined : filters.type, mediaOnly: filters.mediaOnly }); setShowExport(false); setToast(`服务端导出完成：${result.fileName}，共 ${result.messageCount.toLocaleString("zh-CN")} 条消息。`); }} />}
    {toast && <Toast onClose={() => setToast(undefined)}>{toast}</Toast>}
  </div>;
}

function mapConversation(row: ServerConversation, index: number): ConversationSummary {
  const title = row.display_name?.trim() || `会话 ${shortIdentifier(row.conversation_id)}`;
  const accents: ConversationSummary["accent"][] = ["blue", "slate", "violet", "amber", "teal", "orange"];
  return {
    id: row.conversation_id,
    title,
    initials: Array.from(title).slice(0, 2).join(""),
    accent: accents[index % accents.length],
    lastMessage: `已归档 ${row.message_count.toLocaleString("zh-CN")} 条消息`,
    lastAt: formatConversationTime(row.last_message_at),
    unread: 0,
    messageCount: row.message_count,
    mediaCount: row.media_count,
    participantCount: row.participant_count,
  };
}

function mapMessage(row: ServerMessage): MessageItem {
  const senderId = row.sender_id || "system";
  const senderName = row.sender_id ? `成员 ${shortIdentifier(row.sender_id)}` : "系统";
  const media = row.media[0];
  return {
    id: row.stable_message_id,
    conversationId: row.conversation_id,
    senderId,
    senderName,
    senderInitial: Array.from(senderName).at(-1) || "系",
    sentAt: row.sent_at,
    timeLabel: new Intl.DateTimeFormat("zh-CN", { hour: "2-digit", minute: "2-digit", hour12: false }).format(new Date(row.sent_at)),
    direction: row.direction,
    type: row.message_type,
    body: row.body_text || (row.message_type === "unsupported" ? `[暂不支持的消息类型：${row.raw_type}]` : undefined),
    quote: row.quoted_message_id ? { sender: "引用消息", time: "", body: shortIdentifier(row.quoted_message_id) } : undefined,
    attachment: media ? {
      name: media.original_name || `${row.message_type} 媒体`,
      meta: [formatBytes(media.size_bytes), media.integrity === "verified" ? "完整" : "不可用"].filter(Boolean).join(" · "),
      kind: mediaKind(row.message_type, media.mime_type),
    } : undefined,
    lifecycle: row.lifecycle === "active" ? "active" : "recalled",
  };
}

function mediaKind(messageType: ServerMessage["message_type"], mime?: string): "document" | "image" | "audio" | "video" {
  if (messageType === "image" || mime?.startsWith("image/")) return "image";
  if (messageType === "audio" || mime?.startsWith("audio/")) return "audio";
  if (messageType === "video" || mime?.startsWith("video/")) return "video";
  return "document";
}

function shortIdentifier(value: string): string {
  return value.length <= 8 ? value : `${value.slice(0, 4)}…${value.slice(-3)}`;
}

function formatConversationTime(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "--";
  return new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit" }).format(date);
}

function formatBytes(value?: number): string {
  if (value === undefined) return "";
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${(value / 1024 / 1024).toFixed(1)} MB`;
}
