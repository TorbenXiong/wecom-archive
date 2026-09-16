import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import { LoaderCircle } from "lucide-react";
import { ConversationList } from "./components/ConversationList";
import { DetailPanel } from "./components/DetailPanel";
import { LocalExportPage } from "./components/LocalExportPage";
import { MessageTimeline } from "./components/MessageTimeline";
import { Navigation } from "./components/Navigation";
import { ExportDialog, LocalCollectionDialog, ServerAccessGate, Toast } from "./components/Overlays";
import { SettingsPage } from "./components/SettingsPage";
import { ServerConfigPage } from "./components/ServerConfigPage";
import type { ConversationSummary, ExportFormat, ExportScope, FilterState, MessageItem, ParticipantItem } from "./domain/types";
import {
  collectLocalArchive,
  countMessages,
  createServerExport,
  getArchiveSummary,
  importClientJson,
  importEnterprisePackage,
  listConversationParticipants,
  listConversations,
  listMessages,
  openExportDirectory,
  type ServerArchiveSummary,
  type ServerConversation,
  type ServerExportResult,
  type ServerMessage,
} from "./lib/server-api";
import "./styles.css";

type Section = "conversations" | "local-export" | "server-config" | "settings";

const defaultFilters: FilterState = { search: "", date: "all", participant: "all", type: "all", mediaOnly: false };
const emptySummary: ServerArchiveSummary = { conversation_count: 0, message_count: 0, media_count: 0, revision: 0, local_collection_available: false };
const DEFAULT_MESSAGE_PAGE_SIZE = 200;
const emptyConversation: ConversationSummary = {
  id: "",
  title: "尚无归档数据",
  initials: "档",
  accent: "blue",
  lastMessage: "等待本机或专属采集端数据",
  lastAt: "--",
  unread: 0,
  messageCount: 0,
  mediaCount: 0,
  participantCount: 0,
  isGroup: false,
};

export default function App() {
  const [token, setToken] = useState<string>();
  // Decide before the first render so desktop auto-connect never flashes the access form.
  const [autoConnecting, setAutoConnecting] = useState(() =>
    (new URLSearchParams(window.location.hash.slice(1)).get("token")?.length ?? 0) >= 24,
  );
  const [summary, setSummary] = useState(emptySummary);
  const [conversations, setConversations] = useState<ConversationSummary[]>([]);
  const [messages, setMessages] = useState<MessageItem[]>([]);
  const [messagePage, setMessagePage] = useState(0);
  const [messagePageSize, setMessagePageSize] = useState(DEFAULT_MESSAGE_PAGE_SIZE);
  const [messageTotal, setMessageTotal] = useState(0);
  const [participants, setParticipants] = useState<ParticipantItem[]>([]);
  const [section, setSection] = useState<Section>("conversations");
  const [selectedId, setSelectedId] = useState("");
  const [filters, setFilters] = useState(defaultFilters);
  const [scope, setScope] = useState<ExportScope>("current_conversation");
  const [format, setFormat] = useState<ExportFormat>("json");
  const [exportDataRedaction, setExportDataRedaction] = useState(false);
  const [showExport, setShowExport] = useState(false);
  const [detailCollapsed, setDetailCollapsed] = useState(false);
  const [messageStatus, setMessageStatus] = useState("等待选择会话");
  const [toast, setToast] = useState<{ message: string; tone: "success" | "error"; actionLabel?: string; onAction?: () => void }>();
  const [importing, setImporting] = useState(false);
  const [collectingLocal, setCollectingLocal] = useState(false);
  const [refreshVersion, setRefreshVersion] = useState(0);
  const archiveRevision = useRef(0);
  const deferredSearch = useDeferredValue(filters.search.trim());

  const loadDirectory = async (accessToken: string) => {
    const [nextSummary, rows] = await Promise.all([
      getArchiveSummary(accessToken),
      listConversations(accessToken),
    ]);
    const nextConversations = rows.map(mapConversation);
    const changed = nextSummary.revision !== archiveRevision.current;
    archiveRevision.current = nextSummary.revision;
    setSummary(nextSummary);
    setConversations(nextConversations);
    setSelectedId((current) => nextConversations.some((item) => item.id === current) ? current : (nextConversations[0]?.id ?? ""));
    return changed;
  };

  useEffect(() => {
    if (!token || section !== "conversations") return;
    const timer = window.setInterval(() => loadDirectory(token)
      .then((changed) => { if (changed) setRefreshVersion((current) => current + 1); })
      .catch(() => undefined), 5000);
    return () => window.clearInterval(timer);
  }, [section, token]);

  const connect = async (accessToken: string) => {
    await loadDirectory(accessToken);
    setToken(accessToken);
  };

  const importPackage = async (file: File) => {
    if (!token || importing) return;
    const lowerName = file.name.toLowerCase();
    if (!lowerName.endsWith(".wca") && !lowerName.endsWith(".json")) {
      setToast({ message: "只支持导入采集端生成的 .wca 加密包或本机导出的 JSON。", tone: "error" });
      return;
    }
    setImporting(true);
    try {
      const result = lowerName.endsWith(".json")
        ? await importClientJson(file, token)
        : await importEnterprisePackage(file, token);
      await loadDirectory(token);
      setRefreshVersion((current) => current + 1);
      setToast({ message: `会话导入完成：新增 ${result.inserted.toLocaleString("zh-CN")} 条消息。`, tone: "success" });
    } catch (error) {
      setToast({ message: error instanceof Error ? error.message : "会话导入失败。", tone: "error" });
    } finally {
      setImporting(false);
    }
  };

  const collectLocal = async () => {
    if (!token || collectingLocal || !summary.local_collection_available) return;
    setCollectingLocal(true);
    try {
      const result = await collectLocalArchive(token);
      await loadDirectory(token);
      setRefreshVersion((current) => current + 1);
      setToast({
        message: `本机采集完成：新增 ${result.inserted.toLocaleString("zh-CN")} 条，更新 ${result.revised.toLocaleString("zh-CN")} 条消息。`,
        tone: "success",
      });
    } catch (error) {
      setToast({ message: error instanceof Error ? error.message : "本机采集未完成。", tone: "error" });
    } finally {
      setCollectingLocal(false);
    }
  };

  const showExportCompleted = (result: ServerExportResult, label: string) => {
    if (!token) return;
    setToast({
      message: `${label}：${result.fileName}，共 ${result.messageCount.toLocaleString("zh-CN")} 条消息。`,
      tone: "success",
      actionLabel: "打开导出目录",
      onAction: () => void openExportDirectory(token).catch((error) => setToast({ message: error instanceof Error ? error.message : "无法打开导出目录。", tone: "error" })),
    });
  };

  useEffect(() => {
    const candidate = new URLSearchParams(window.location.hash.slice(1)).get("token");
    if (!candidate || candidate.length < 24) return;
    void connect(candidate).then(() => {
      window.history.replaceState(null, "", window.location.pathname + window.location.search);
      setAutoConnecting(false);
    }).catch(() => {
      window.history.replaceState(null, "", window.location.pathname + window.location.search);
      setAutoConnecting(false);
    });
  }, []);

  useEffect(() => {
    if (!token || !selectedId) {
      setMessages([]);
      setMessageTotal(0);
      return;
    }
    const controller = new AbortController();
    setMessageStatus("正在读取服务端归档…");
    const query = {
      conversationId: selectedId,
      participantId: filters.participant === "all" ? undefined : filters.participant,
      text: deferredSearch || undefined,
      messageType: filters.type === "all" ? undefined : filters.type,
      mediaOnly: filters.mediaOnly,
    };
    Promise.all([
      listMessages(token, { ...query, limit: messagePageSize, offset: messagePage * messagePageSize }, controller.signal),
      countMessages(token, query, controller.signal),
    ]).then(([rows, count]) => {
      const totalPages = Math.max(1, Math.ceil(count.total / messagePageSize));
      if (messagePage >= totalPages && messagePage > 0) {
        setMessageTotal(count.total);
        setMessagePage(totalPages - 1);
        return;
      }
      setMessageTotal(count.total);
      setMessages(rows.map(mapMessage));
      setMessageStatus(rows.length === 0 ? "当前条件下没有消息" : "");
    }).catch((reason) => {
      if (reason instanceof DOMException && reason.name === "AbortError") return;
      setMessages([]);
      setMessageTotal(0);
      setMessageStatus(reason instanceof Error ? reason.message : "消息读取失败");
    });
    return () => controller.abort();
  }, [deferredSearch, filters.mediaOnly, filters.participant, filters.type, messagePage, messagePageSize, refreshVersion, selectedId, token]);

  useEffect(() => {
    setMessagePage(0);
  }, [deferredSearch, filters.mediaOnly, filters.participant, filters.type, messagePageSize, selectedId]);

  useEffect(() => {
    if (!token || !selectedId) {
      setParticipants([]);
      return;
    }
    const controller = new AbortController();
    listConversationParticipants(token, selectedId, controller.signal)
      .then((rows) => setParticipants(rows.map((row) => ({
        id: row.participant_id,
        name: row.display_name?.trim() || `成员 ${shortIdentifier(row.participant_id)}`,
        kind: row.participant_kind,
      }))))
      .catch((reason) => {
        if (reason instanceof DOMException && reason.name === "AbortError") return;
        setParticipants([]);
      });
    return () => controller.abort();
  }, [refreshVersion, selectedId, token]);

  const selectedConversation = conversations.find((item) => item.id === selectedId) ?? emptyConversation;
  const isFullPageSection = section === "server-config" || section === "settings" || section === "local-export";
  const participantOptions = useMemo(() => {
    const values = new Map<string, string>();
    for (const message of messages) {
      if (message.senderId !== "system") values.set(message.senderId, message.senderName);
    }
    return [...values].map(([id, label]) => ({ id, label }));
  }, [messages]);

  if (!token && autoConnecting) return <div className="access-shell startup-shell">
    <main className="access-main">
      <section className="access-card" role="status">
        <span className="access-icon"><LoaderCircle className="spin" size={28} /></span>
        <h1>正在启动归档工作台</h1>
        <p>正在加载归档工作台，请稍候…</p>
      </section>
    </main>
  </div>;
  if (!token) return <ServerAccessGate onConnect={connect} />;

  return <div className="app-shell">
    <div className={section === "local-export" ? "workspace-grid local-export-mode" : isFullPageSection ? "workspace-grid settings-mode" : !selectedConversation.isGroup ? "workspace-grid no-detail" : detailCollapsed ? "workspace-grid detail-collapsed" : "workspace-grid"}>
      <Navigation active={section} onSelect={setSection} />
      {section === "server-config" ? <ServerConfigPage token={token} onTokenChanged={setToken} /> : section === "settings" ? <SettingsPage /> : section === "local-export" ? <LocalExportPage token={token} onCompleted={(result) => showExportCompleted(result, "本机导出完成")} onFailed={(error) => setToast({ message: error instanceof Error ? error.message : "本机导出失败。", tone: "error" })} /> : <>
        <ConversationList conversations={conversations} selectedId={selectedId} filters={filters} participantOptions={participantOptions} onFiltersChange={setFilters} onSelect={setSelectedId} importing={importing} onImport={(file) => void importPackage(file)} />
        <MessageTimeline token={token} conversation={selectedConversation} messages={messages} participants={participants} status={messageStatus} pageIndex={messagePage} pageSize={messagePageSize} totalMessages={messageTotal} totalPages={Math.max(1, Math.ceil(messageTotal / messagePageSize))} onPageSizeChange={(size) => setMessagePageSize(size)} onPageChange={(page) => setMessagePage(page)} onPreviousPage={() => setMessagePage((current) => Math.max(0, current - 1))} onNextPage={() => setMessagePage((current) => current + 1)} localCollectionAvailable={summary.local_collection_available} collectingLocal={collectingLocal} onCollectLocal={() => void collectLocal()} onExport={() => setShowExport(true)} />
        {selectedConversation.isGroup && <DetailPanel conversation={selectedConversation} participants={participants} messages={messages} collapsed={detailCollapsed} onToggleCollapsed={() => setDetailCollapsed((current) => !current)} />}
      </>}
    </div>
    <footer className="status-bar"><span>已归档 {summary.message_count.toLocaleString("zh-CN")} 条消息 · {summary.media_count.toLocaleString("zh-CN")} 项媒体 · {summary.conversation_count.toLocaleString("zh-CN")} 个会话</span><span><i />受控访问 · 审计已启用</span></footer>
    {showExport && <ExportDialog scope={scope} format={format} dataRedaction={exportDataRedaction} onScopeChange={setScope} onFormatChange={setFormat} onDataRedactionChange={setExportDataRedaction} messageCount={scope === "entire_archive" ? summary.message_count : scope === "current_filter" ? messages.length : selectedConversation.messageCount} mediaCount={scope === "entire_archive" ? summary.media_count : scope === "current_filter" ? messages.filter((message) => message.attachment).length : selectedConversation.mediaCount} onClose={() => setShowExport(false)} onConfirm={async () => { const result = await createServerExport(token, { scope, format, conversationId: selectedId || undefined, participantId: filters.participant === "all" ? undefined : filters.participant, text: filters.search.trim() || undefined, messageType: filters.type === "all" ? undefined : filters.type, mediaOnly: filters.mediaOnly, dataRedaction: exportDataRedaction }); setShowExport(false); showExportCompleted(result, "服务端导出完成"); }} />}
    {collectingLocal && <LocalCollectionDialog />}
    {toast && <Toast tone={toast.tone} actionLabel={toast.actionLabel} onAction={toast.onAction} onClose={() => setToast(undefined)}>{toast.message}</Toast>}
  </div>;
}

function mapConversation(row: ServerConversation, index: number): ConversationSummary {
  const explicitType = row.conversation_type?.trim().toLowerCase();
  // Source conversation prefixes are more reliable than a stale persisted type
  // from archives created before direct/group classification was explicit.
  const isGroup = row.conversation_id.startsWith("R:")
    ? true
    : row.conversation_id.startsWith("S:")
      ? false
      : explicitType === "group";
  const directNames = (row.participant_names || []).map((name) => name.trim()).filter(Boolean);
  const title = !isGroup && directNames.length > 0
    ? directNames.join("、")
    : row.display_name?.trim() || `会话 ${shortIdentifier(row.conversation_id)}`;
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
    isGroup,
  };
}

function mapMessage(row: ServerMessage): MessageItem {
  const senderId = row.sender_id || "system";
  const senderName = row.sender_name?.trim() || (row.sender_id ? `成员 ${shortIdentifier(row.sender_id)}` : "系统");
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
    rawType: row.raw_type,
    body: row.body_text || (row.message_type === "unsupported" ? `[暂不支持的消息类型：${row.raw_type}]` : undefined),
    quote: row.quoted_message_id ? {
      id: row.quoted_message?.stable_message_id || row.quoted_message_id,
      sender: row.quoted_message?.sender_name?.trim() || (row.quoted_message?.sender_id ? `成员 ${shortIdentifier(row.quoted_message.sender_id)}` : "原消息"),
      time: row.quoted_message ? formatMessageDateTime(row.quoted_message.sent_at) : "",
      body: row.quoted_message?.body_text || `[${messageTypeLabel(row.quoted_message?.message_type)}]`,
    } : undefined,
    attachment: media ? {
      name: media.original_name || `${row.message_type} 媒体`,
      meta: [formatBytes(media.size_bytes), media.integrity === "verified" ? "完整" : "不可用"].filter(Boolean).join(" · "),
      kind: mediaKind(row.message_type, media.mime_type),
      contentHash: media.content_hash,
    } : undefined,
    lifecycle: row.lifecycle === "active" ? "active" : "recalled",
  };
}

function formatMessageDateTime(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  return new Intl.DateTimeFormat("zh-CN", { month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", hour12: false }).format(date);
}

function messageTypeLabel(value?: ServerMessage["message_type"]): string {
  const labels: Record<ServerMessage["message_type"], string> = {
    text: "文本消息", image: "图片", audio: "语音", video: "视频", file: "文件", link: "链接", reply: "引用消息", system: "系统消息", unsupported: "暂不支持的消息",
  };
  return value ? labels[value] : "原消息不可用";
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
