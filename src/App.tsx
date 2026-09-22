import { useEffect, useRef, useState } from "react";
import { LoaderCircle, ShieldCheck } from "lucide-react";
import { ConversationList } from "./components/ConversationList";
import { CollectionSchedulePage } from "./components/CollectionSchedulePage";
import { DetailPanel } from "./components/DetailPanel";
import { LocalExportPage } from "./components/LocalExportPage";
import { MessageTimeline } from "./components/MessageTimeline";
import { Navigation } from "./components/Navigation";
import { ExportDialog, LocalCollectionDialog, LocalCollectionSetupDialog, ServerAccessGate, Toast } from "./components/Overlays";
import { SettingsPage } from "./components/SettingsPage";
import { ServerConfigPage } from "./components/ServerConfigPage";
import type { ConversationSummary, ExportFormat, ExportScope, MessageItem, ParticipantItem } from "./domain/types";
import {
  collectLocalArchive,
  countMessages,
  createServerExport,
  getArchiveSummary,
  waitForArchiveUpdate,
  getCollectedUsers,
  getEnterpriseConfig,
  getLocalCollectionProgress,
  importClientJson,
  importEnterprisePackage,
  listConversationParticipants,
  listConversations,
  listMessages,
  openExportDirectory,
  type ExportOrder,
  type ServerArchiveSummary,
  type ServerConversation,
  type ServerExportResult,
  type GlobalSearchResult,
  type LocalCollectionProgress,
  type ServerMessage,
  type CollectedUser,
  ServerApiError,
} from "./lib/server-api";
import "./styles.css";

type Section = "conversations" | "local-export" | "server-config" | "collection-schedules" | "settings";

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
  const [superAdminEnabled, setSuperAdminEnabled] = useState(true);
  const [collectedUsers, setCollectedUsers] = useState<CollectedUser[]>([]);
  const [conversations, setConversations] = useState<ConversationSummary[]>([]);
  const [messages, setMessages] = useState<MessageItem[]>([]);
  const [messagePage, setMessagePage] = useState(0);
  const [messagePageSize, setMessagePageSize] = useState(DEFAULT_MESSAGE_PAGE_SIZE);
  const [messageTotal, setMessageTotal] = useState(0);
  const [messageSortAscending, setMessageSortAscending] = useState(false);
  const [participants, setParticipants] = useState<ParticipantItem[]>([]);
  const [section, setSection] = useState<Section>("local-export");
  const [selectedId, setSelectedId] = useState("");
  const [targetMessageId, setTargetMessageId] = useState<string>();
  const [scope, setScope] = useState<ExportScope>("current_conversation");
  const [format, setFormat] = useState<ExportFormat>("json");
  const [exportDataRedaction, setExportDataRedaction] = useState(true);
  const [exportSimplify, setExportSimplify] = useState(true);
  const [exportPretty, setExportPretty] = useState(true);
  const [exportConversationOrder, setExportConversationOrder] = useState<ExportOrder>("descending");
  const [exportMessageOrder, setExportMessageOrder] = useState<ExportOrder>("descending");
  const [showExport, setShowExport] = useState(false);
  const [detailCollapsed, setDetailCollapsed] = useState(false);
  const [messageStatus, setMessageStatus] = useState("等待选择会话");
  const [toast, setToast] = useState<{ message: string; tone: "success" | "error"; actionLabel?: string; onAction?: () => void }>();
  const [importing, setImporting] = useState(false);
  const [collectingLocal, setCollectingLocal] = useState(false);
  const [showLocalCollectionSetup, setShowLocalCollectionSetup] = useState(false);
  const [collectionProgress, setCollectionProgress] = useState<LocalCollectionProgress>({ running: false, percent: 0, stage: "准备采集", detail: "正在初始化本机采集任务。" });
  const [refreshVersion, setRefreshVersion] = useState(0);
  const archiveRevision = useRef(0);
  const skipNextPageReset = useRef(false);
  const selectedConversationOverride = useRef<ConversationSummary | undefined>(undefined);

  const loadDirectory = async (accessToken: string) => {
    const [nextSummary, rows] = await Promise.all([
      getArchiveSummary(accessToken),
      listConversations(accessToken),
    ]);
    const nextConversations = rows.map(mapConversation);
    const override = selectedConversationOverride.current;
    if (override && !nextConversations.some((item) => item.id === override.id)) nextConversations.push(override);
    const changed = nextSummary.revision !== archiveRevision.current;
    archiveRevision.current = nextSummary.revision;
    setSummary(nextSummary);
    setConversations(nextConversations);
    setSelectedId((current) => nextConversations.some((item) => item.id === current) ? current : (nextConversations[0]?.id ?? ""));
    return changed;
  };

  useEffect(() => {
    if (!token || section !== "conversations") return;
    const controller = new AbortController();
    let active = true;

    const listenForArchiveUpdates = async () => {
      while (active && !controller.signal.aborted) {
        try {
          const update = await waitForArchiveUpdate(token, archiveRevision.current, controller.signal);
          if (!active) return;
          if (update.revision !== archiveRevision.current) {
            const changed = await loadDirectory(token);
            if (changed) setRefreshVersion((current) => current + 1);
          }
        } catch (reason) {
          if (reason instanceof DOMException && reason.name === "AbortError") return;
          // 仅在连接异常时重试监听，不主动刷新归档数据。
          await new Promise((resolve) => window.setTimeout(resolve, 1000));
        }
      }
    };

    void listenForArchiveUpdates();
    return () => {
      active = false;
      controller.abort();
    };
  }, [section, token]);

  const connect = async (accessToken: string) => {
    const [, config] = await Promise.all([
      loadDirectory(accessToken),
      getEnterpriseConfig(accessToken).catch((error) => {
        if (error instanceof ServerApiError && error.status === 404) return undefined;
        throw error;
      }),
    ]);
    // Older embedded test/API hosts may not expose the optional settings endpoint;
    // preserve their existing full-content behavior while the real server returns
    // an explicit default of false.
    setSuperAdminEnabled(config?.superAdminEnabled ?? true);
    setToken(accessToken);
  };

  useEffect(() => {
    if (!token) return;
    const controller = new AbortController();
    getCollectedUsers(token, controller.signal)
      .then(setCollectedUsers)
      .catch((reason) => {
        if (!(reason instanceof DOMException && reason.name === "AbortError")) setCollectedUsers([]);
      });
    return () => controller.abort();
  }, [refreshVersion, token]);

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

  const collectLocal = async (includeMedia: boolean) => {
    if (!token || collectingLocal || !summary.local_collection_available) return;
    setCollectingLocal(true);
    setCollectionProgress({ running: true, percent: 2, stage: "准备采集", detail: includeMedia ? "正在准备采集文本、图片和文件。" : "正在准备仅采集文本消息。" });
    try {
      const result = await collectLocalArchive(token, includeMedia);
      await loadDirectory(token);
      setRefreshVersion((current) => current + 1);
      setToast({
        message: `本机采集完成：新增 ${result.inserted.toLocaleString("zh-CN")} 条，更新 ${result.revised.toLocaleString("zh-CN")} 条消息${includeMedia ? `；采集媒体 ${result.mediaCount.toLocaleString("zh-CN")} 项${result.missingMediaCount > 0 ? `，${result.missingMediaCount.toLocaleString("zh-CN")} 项源文件未找到` : ""}` : ""}。`,
        tone: "success",
      });
    } catch (error) {
      setToast({ message: error instanceof Error ? error.message : "本机采集未完成。", tone: "error" });
    } finally {
      setCollectingLocal(false);
    }
  };

  const openLocalCollectionSetup = () => {
    if (!token || collectingLocal) return;
    setShowLocalCollectionSetup(true);
  };

  useEffect(() => {
    if (!token || !collectingLocal) return;
    const controller = new AbortController();
    let polling = false;
    const refreshProgress = async () => {
      if (polling) return;
      polling = true;
      try {
        setCollectionProgress(await getLocalCollectionProgress(token, controller.signal));
      } catch (error) {
        if (error instanceof DOMException && error.name === "AbortError") return;
      } finally {
        polling = false;
      }
    };
    void refreshProgress();
    const interval = window.setInterval(() => void refreshProgress(), 350);
    return () => {
      controller.abort();
      window.clearInterval(interval);
    };
  }, [collectingLocal, token]);

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
    if (!token || !selectedId || !superAdminEnabled) {
      setMessages([]);
      setMessageTotal(0);
      return;
    }
    const controller = new AbortController();
    setMessageStatus("正在读取服务端归档…");
    const query = { conversationId: selectedId };
    Promise.all([
      listMessages(token, { ...query, limit: messagePageSize, offset: messagePage * messagePageSize, sort: messageSortAscending ? "asc" : "desc" }, controller.signal),
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
  }, [messagePage, messagePageSize, messageSortAscending, refreshVersion, selectedId, superAdminEnabled, token]);

  useEffect(() => {
    if (skipNextPageReset.current) {
      skipNextPageReset.current = false;
      return;
    }
    setMessagePage(0);
    setTargetMessageId(undefined);
  }, [messagePageSize, messageSortAscending, selectedId]);

  useEffect(() => {
    if (!token || !selectedId || !superAdminEnabled) {
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
  }, [refreshVersion, selectedId, superAdminEnabled, token]);

  const selectedConversation = conversations.find((item) => item.id === selectedId) ?? emptyConversation;
  const isFullPageSection = section === "server-config" || section === "collection-schedules" || section === "settings" || section === "local-export";
  const openSearchResult = (result: GlobalSearchResult) => {
    if (!conversations.some((conversation) => conversation.id === result.conversation_id)) {
      const conversation = mapSearchConversation(result);
      selectedConversationOverride.current = conversation;
      setConversations((current) => [...current, conversation]);
    }
    skipNextPageReset.current = result.conversation_id !== selectedId;
    setSelectedId(result.conversation_id);
    setMessagePage(Math.floor(result.offset_in_conversation / messagePageSize));
    setTargetMessageId(result.stable_message_id);
  };

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
    <div className={section === "local-export" ? "workspace-grid local-export-mode" : isFullPageSection ? "workspace-grid settings-mode" : !superAdminEnabled || !selectedConversation.isGroup ? "workspace-grid no-detail" : detailCollapsed ? "workspace-grid detail-collapsed" : "workspace-grid"}>
      <Navigation active={section} onSelect={setSection} />
      {section === "server-config" ? <ServerConfigPage token={token} onTokenChanged={setToken} /> : section === "collection-schedules" ? <CollectionSchedulePage token={token} /> : section === "settings" ? <SettingsPage token={token} superAdminEnabled={superAdminEnabled} onSuperAdminChange={setSuperAdminEnabled} /> : section === "local-export" ? <LocalExportPage token={token} onCompleted={(result) => showExportCompleted(result, "本机导出完成")} onFailed={(error) => setToast({ message: error instanceof Error ? error.message : "本机导出失败。", tone: "error" })} /> : <>
        <ConversationList token={token} conversations={conversations} selectedId={selectedId} messageSortAscending={messageSortAscending} superAdminEnabled={superAdminEnabled} collectedUsers={collectedUsers} onSelect={(id) => { selectedConversationOverride.current = undefined; setTargetMessageId(undefined); setSelectedId(id); }} onOpenSearchResult={openSearchResult} onExport={() => setShowExport(true)} localCollectionAvailable={summary.local_collection_available} collectingLocal={collectingLocal} onOpenLocalCollection={openLocalCollectionSetup} importing={importing} onImport={(file) => void importPackage(file)} />
        {superAdminEnabled ? <MessageTimeline token={token} conversation={selectedConversation} messages={messages} participants={participants} sortAscending={messageSortAscending} onSortChange={(ascending) => { setMessageSortAscending(ascending); setMessagePage(0); }} status={messageStatus} targetMessageId={targetMessageId} onTargetLocated={() => setTargetMessageId(undefined)} pageIndex={messagePage} pageSize={messagePageSize} totalMessages={messageTotal} totalPages={Math.max(1, Math.ceil(messageTotal / messagePageSize))} onPageSizeChange={(size) => setMessagePageSize(size)} onPageChange={(page) => setMessagePage(page)} onPreviousPage={() => setMessagePage((current) => Math.max(0, current - 1))} onNextPage={() => setMessagePage((current) => current + 1)} /> : <main className="content-gate"><ShieldCheck size={28} /><h2>会话内容已隐藏</h2></main>}
        {superAdminEnabled && selectedConversation.isGroup && <DetailPanel conversation={selectedConversation} participants={participants} messages={messages} collapsed={detailCollapsed} onToggleCollapsed={() => setDetailCollapsed((current) => !current)} />}
      </>}
    </div>
    <footer className="status-bar"><span>已归档 {summary.message_count.toLocaleString("zh-CN")} 条消息 · {summary.media_count.toLocaleString("zh-CN")} 项媒体 · {summary.conversation_count.toLocaleString("zh-CN")} 个会话</span><span><i />受控访问 · 审计已启用</span></footer>
     {showExport && superAdminEnabled && <ExportDialog token={token} scope={scope} format={format} dataRedaction={exportDataRedaction} simplify={exportSimplify} pretty={exportPretty} conversationOrder={exportConversationOrder} messageOrder={exportMessageOrder} onConversationOrderChange={setExportConversationOrder} onMessageOrderChange={setExportMessageOrder} onSimplifyChange={setExportSimplify} onPrettyChange={setExportPretty} onScopeChange={setScope} onFormatChange={setFormat} onDataRedactionChange={setExportDataRedaction} messageCount={scope === "entire_archive" ? summary.message_count : scope === "current_filter" ? messages.length : selectedConversation.messageCount} mediaCount={scope === "entire_archive" ? summary.media_count : scope === "current_filter" ? messages.filter((message) => message.attachment).length : selectedConversation.mediaCount} onOpenDirectoryFailed={(error) => setToast({ message: error instanceof Error ? error.message : "无法打开导出目录。", tone: "error" })} onClose={() => setShowExport(false)} onConfirm={async () => { const result = await createServerExport(token, { scope, format, conversationId: selectedId || undefined, dataRedaction: exportDataRedaction, simplify: exportSimplify, pretty: exportPretty, conversationOrder: exportConversationOrder, messageOrder: exportMessageOrder }); setShowExport(false); showExportCompleted(result, "服务端导出完成"); }} />}
    {showLocalCollectionSetup && !collectingLocal && <LocalCollectionSetupDialog onClose={() => setShowLocalCollectionSetup(false)} onCollect={(includeMedia) => { setShowLocalCollectionSetup(false); void collectLocal(includeMedia); }} />}
    {collectingLocal && <LocalCollectionDialog progress={collectionProgress} />}
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

function mapSearchConversation(result: GlobalSearchResult): ConversationSummary {
  const title = result.conversation_name?.trim() || `会话 ${shortIdentifier(result.conversation_id)}`;
  return {
    id: result.conversation_id,
    title,
    initials: Array.from(title).slice(0, 2).join(""),
    accent: "blue",
    lastMessage: "来自全局搜索",
    lastAt: formatConversationTime(result.sent_at),
    unread: 0,
    messageCount: 0,
    mediaCount: 0,
    participantCount: 0,
    isGroup: result.conversation_id.startsWith("R:"),
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
