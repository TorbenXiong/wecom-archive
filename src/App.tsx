import { useEffect, useMemo, useRef, useState } from "react";
import { LoaderCircle, ShieldCheck } from "lucide-react";
import { ConversationList } from "./components/ConversationList";
import { DetailPanel } from "./components/DetailPanel";
import { LocalExportPage } from "./components/LocalExportPage";
import { MessageTimeline } from "./components/MessageTimeline";
import { Navigation } from "./components/Navigation";
import { ArchiveHealthPage } from "./components/ArchiveHealthPage";
import { TaskCenterPage } from "./components/TaskCenterPage";
import { CustomSourceRootDialog, ExportDialog, LocalCollectionDialog, LocalCollectionSetupDialog, ServerAccessGate, Toast, type ExportSelection } from "./components/Overlays";
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
  getMessagePosition,
  importClientJson,
  importEnterprisePackage,
  listConversationParticipants,
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
import { useConversationPages } from "./lib/use-conversation-pages";
import { ArchiveTaskStatus } from "./components/ArchiveTaskStatus";
import { ArchiveTaskPendingError, type ArchiveTaskKind, type ArchiveTaskProgress } from "./lib/long-task";
import { resumeArchiveTaskStatus, type ServerImportResult } from "./lib/server-api";
import "./styles.css";

type Section = "conversations" | "local-export" | "server-config" | "settings" | "archive-health" | "tasks";

type PageAnchor = { context: string; page: number; id: string; shift: number };
function messagePageContext(token: string | undefined, conversation: string, size: number, ascending: boolean, revision: number): string {
  return JSON.stringify([token, conversation, size, ascending, revision]);
}

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
  const conversationPages = useConversationPages();
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
  const [importProgress, setImportProgress] = useState<ArchiveTaskProgress>();
  const [collectionTask, setCollectionTask] = useState<ArchiveTaskProgress>();
  const [pendingImport, setPendingImport] = useState<string>();
  const [pendingCollection, setPendingCollection] = useState<string>();
  const activeToken = useRef(token);
  activeToken.current = token;
  useEffect(() => {
    setImportProgress(undefined); setCollectionTask(undefined);
    setPendingImport(undefined); setPendingCollection(undefined);
    setImporting(false); setCollectingLocal(false);
  }, [token]);
  const [localCollectionDataRedaction, setLocalCollectionDataRedaction] = useState(true);
  const [showLocalCollectionSetup, setShowLocalCollectionSetup] = useState(false);
  const [collectionProgress, setCollectionProgress] = useState<LocalCollectionProgress>({ running: false, percent: 0, stage: "准备采集", detail: "正在初始化本机采集任务。" });
  const [refreshVersion, setRefreshVersion] = useState(0);
  const lastMessagePage = useRef<{ context: string; page: number; first: string; last: string } | undefined>(undefined);
  const pendingPageAnchor = useRef<PageAnchor | undefined>(undefined);
  const pageContext = messagePageContext(token, selectedId, messagePageSize, messageSortAscending, summary.revision);
  const currentPageRequest = useRef({ context: pageContext, page: messagePage });
  currentPageRequest.current = { context: pageContext, page: messagePage };
  const archiveRevision = useRef(0);
  const directoryRequest = useRef(0);
  const skipNextPageReset = useRef(false);
  const selectedConversationOverride = useRef<ConversationSummary | undefined>(undefined);
  const conversations = useMemo(() => {
    const rows = conversationPages.rows.map(mapConversation);
    const override = selectedConversationOverride.current;
    if (override && !rows.some((item) => item.id === override.id)) rows.push(override);
    return rows;
  }, [conversationPages.rows, selectedId]);

  const loadDirectory = async (accessToken: string, signal?: AbortSignal) => {
    const request = ++directoryRequest.current;
    const [nextSummary, rows] = await Promise.all([
      getArchiveSummary(accessToken),
      conversationPages.reset(accessToken, signal),
    ]);
    if (request !== directoryRequest.current || signal?.aborted) return false;
    const nextConversations = rows.map(mapConversation);
    const override = selectedConversationOverride.current;
    if (override && !nextConversations.some((item) => item.id === override.id)) nextConversations.push(override);
    const changed = nextSummary.revision !== archiveRevision.current;
    archiveRevision.current = nextSummary.revision;
    setSummary(nextSummary);
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
            const changed = await loadDirectory(token, controller.signal);
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
    setLocalCollectionDataRedaction(config?.dataRedaction ?? true);
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

  const completeArchiveTask = async (accessToken: string, result: ServerImportResult, label: string, successMessage?: string) => {
    if (activeToken.current !== accessToken) return;
    try { await loadDirectory(accessToken); }
    catch {
      if (activeToken.current === accessToken) setToast({ message: `${label}已完成，但列表暂时无法刷新，请稍后重新加载。`, tone: "error" });
      return;
    }
    if (activeToken.current === accessToken) {
      setRefreshVersion((current) => current + 1);
      setToast({ message: successMessage || `${label}完成：新增 ${result.inserted.toLocaleString("zh-CN")} 条，更新 ${result.revised.toLocaleString("zh-CN")} 条消息。`, tone: "success" });
    }
  };

  const importPackage = async (file: File) => {
    if (!token || importing || pendingImport) return;
    const lowerName = file.name.toLowerCase();
    if (!lowerName.endsWith(".wca") && !lowerName.endsWith(".json")) {
      setToast({ message: "只支持导入采集端生成的 .wca 加密包或本机导出的 JSON。", tone: "error" });
      return;
    }
    setImporting(true);
    try {
      const result = lowerName.endsWith(".json")
        ? await importClientJson(file, token, (progress) => { if (activeToken.current === token) setImportProgress(progress); })
        : await importEnterprisePackage(file, token, (progress) => { if (activeToken.current === token) setImportProgress(progress); });
      if (activeToken.current !== token) return;
      setImportProgress(undefined);
      await completeArchiveTask(token, result, "会话导入", `会话导入完成：新增 ${result.inserted.toLocaleString("zh-CN")} 条消息。`);
    } catch (error) {
      if (activeToken.current !== token) return;
      if (error instanceof ArchiveTaskPendingError) { setPendingImport(error.taskId); return; }
      setImportProgress(undefined);
      setToast({ message: error instanceof Error ? error.message : "会话导入失败。", tone: "error" });
    } finally {
      if (activeToken.current === token) setImporting(false);
    }
  };

  const [customRootIssue, setCustomRootIssue] = useState<{ includeMedia: boolean; message: string; attemptedRoot?: string } | null>(null);

  const collectLocal = async (includeMedia: boolean, sourceRoot?: string) => {
    if (!token || collectingLocal || pendingCollection || !summary.local_collection_available) return;
    setCollectingLocal(true);
    setCollectionProgress({ running: true, percent: 2, stage: "准备采集", detail: includeMedia ? "正在准备采集文本、图片和文件。" : "正在准备仅采集文本消息。" });
    try {
      const result = await collectLocalArchive(token, includeMedia, sourceRoot, localCollectionDataRedaction, (progress) => { if (activeToken.current === token) setCollectionTask(progress); });
      if (activeToken.current !== token) return;
      setCustomRootIssue(null);
      setCollectionTask(undefined);
      await completeArchiveTask(token, result, includeMedia ? "本机全内容采集" : "本机采集", includeMedia
        ? `本机全内容采集完成：新增 ${result.inserted.toLocaleString("zh-CN")} 条，图片和附件已处理。`
        : `本机采集完成：新增 ${result.inserted.toLocaleString("zh-CN")} 条，更新 ${result.revised.toLocaleString("zh-CN")} 条消息。`);
    } catch (error) {
      if (activeToken.current !== token) return;
      if (error instanceof ArchiveTaskPendingError) { setPendingCollection(error.taskId); return; }
      setCollectionTask(undefined);
      setToast({ message: error instanceof Error ? error.message : "本机采集未完成。", tone: "error" });
      if (error instanceof ServerApiError && error.code === "LOCAL_SOURCE_NOT_FOUND") {
        setCustomRootIssue({ includeMedia, message: error.message, attemptedRoot: sourceRoot });
      } else {
        setCustomRootIssue(null);
      }
    } finally {
      if (activeToken.current === token) setCollectingLocal(false);
    }
  };

  const openLocalCollectionSetup = () => {
    if (!token || collectingLocal || pendingCollection) return;
    setShowLocalCollectionSetup(true);
  };

  const openLocalCollectionFromConversation = () => {
    setSection("server-config");
    openLocalCollectionSetup();
  };

  const recheckArchiveTask = async (kind: ArchiveTaskKind) => {
    const id = kind === "import" ? pendingImport : pendingCollection;
    if (!token || !id || (kind === "import" ? importing : collectingLocal)) return;
    const setBusy = kind === "import" ? setImporting : setCollectingLocal;
    const setProgress = kind === "import" ? setImportProgress : setCollectionTask;
    const clear = () => {
      setProgress(undefined);
      if (kind === "import") setPendingImport(undefined);
      else setPendingCollection(undefined);
    };
    setBusy(true);
    try {
      const result = await resumeArchiveTaskStatus(token, id, kind, (progress) => { if (activeToken.current === token) setProgress(progress); });
      if (activeToken.current !== token) return;
      clear();
      await completeArchiveTask(token, result, kind === "import" ? "会话导入" : "本机采集");
    } catch (error) {
      if (activeToken.current !== token || error instanceof ArchiveTaskPendingError) return;
      clear();
      setToast({ message: error instanceof Error ? error.message : "任务未完成。", tone: "error" });
    } finally { if (activeToken.current === token) setBusy(false); }
  };

  useEffect(() => {
    if (!token || (!collectingLocal && !collectionProgress.running)) return;
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
  }, [collectingLocal, collectionProgress.running, token]);

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
      lastMessagePage.current = undefined;
      pendingPageAnchor.current = undefined;
      return;
    }
    const controller = new AbortController();
    setMessageStatus("正在读取服务端归档…");
    const isCurrentPage = () => !controller.signal.aborted
      && currentPageRequest.current.context === pageContext && currentPageRequest.current.page === messagePage;
    const pending = pendingPageAnchor.current;
    const previous = lastMessagePage.current;
    let anchor: Pick<PageAnchor, "id" | "shift"> | undefined;
    if (pending?.context === pageContext && pending.page === messagePage) {
      anchor = pending;
    } else {
      pendingPageAnchor.current = undefined;
      if (previous?.context === pageContext) {
        if (messagePage === previous.page + 1) anchor = { id: previous.last, shift: 1 };
        else if (messagePage === previous.page - 1) anchor = { id: previous.first, shift: -messagePageSize };
        else if (messagePage === previous.page) anchor = { id: previous.first, shift: 0 };
      }
    }
    const query = { conversationId: selectedId };
    Promise.all([
      listMessages(token, { ...query, limit: messagePageSize, offset: messagePage * messagePageSize, sort: messageSortAscending ? "asc" : "desc", ...(anchor ? { anchorMessageId: anchor.id, anchorShift: anchor.shift, anchorRevision: summary.revision } : {}) }, controller.signal),
      countMessages(token, query, controller.signal),
    ]).then(([rows, count]) => {
      if (!isCurrentPage()) return;
      const totalPages = Math.max(1, Math.ceil(count.total / messagePageSize));
      if (messagePage >= totalPages && messagePage > 0) {
        setMessageTotal(count.total);
        setMessagePage(totalPages - 1);
        return;
      }
      setMessageTotal(count.total);
      setMessages(rows.map(mapMessage));
      lastMessagePage.current = rows.length > 0 ? { context: pageContext, page: messagePage, first: rows[0].stable_message_id, last: rows[rows.length - 1].stable_message_id } : undefined;
      if (pendingPageAnchor.current === pending) pendingPageAnchor.current = undefined;
      setMessageStatus(rows.length === 0 ? "当前条件下没有消息" : "");
    }).catch((reason) => {
      if (!isCurrentPage() || reason instanceof DOMException && reason.name === "AbortError") return;
      setMessages([]);
      setMessageTotal(0);
      setMessageStatus(reason instanceof Error ? reason.message : "消息读取失败");
    });
    return () => controller.abort();
  }, [messagePage, messagePageSize, messageSortAscending, pageContext, refreshVersion, selectedId, summary.revision, superAdminEnabled, token]);

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
        profile: row.profile,
      }))))
      .catch((reason) => {
        if (reason instanceof DOMException && reason.name === "AbortError") return;
        setParticipants([]);
      });
    return () => controller.abort();
  }, [refreshVersion, selectedId, superAdminEnabled, token]);

  const selectedConversation = conversations.find((item) => item.id === selectedId) ?? emptyConversation;
  useEffect(() => {
    if (selectedConversation.id) selectedConversationOverride.current = selectedConversation;
  }, [selectedConversation]);
  const conversationPaging = {
    hasMore: conversationPages.hasMore,
    loading: conversationPages.loading,
    error: conversationPages.error,
    total: summary.conversation_count,
    onLoadMore: () => { void conversationPages.loadMore(token); },
  };
  const isFullPageSection = section !== "conversations";
  const openSearchResult = async (result: GlobalSearchResult, signal?: AbortSignal) => {
    if (!token) return;
    const accessToken = token;
    const offset = result.offset_in_conversation ?? (await getMessagePosition(
      accessToken, result.stable_message_id, messageSortAscending ? "asc" : "desc", signal,
    )).offset_in_conversation;
    if (signal?.aborted || currentPageRequest.current.context !== pageContext || currentPageRequest.current.page !== messagePage) return;
    if (!Number.isSafeInteger(offset) || offset < 0) throw new Error("无法定位原消息，请重试。");
    selectedConversationOverride.current = conversations.find((item) => item.id === result.conversation_id) ?? mapSearchConversation(result);
    skipNextPageReset.current = result.conversation_id !== selectedId;
    pendingPageAnchor.current = { context: messagePageContext(token, result.conversation_id, messagePageSize, messageSortAscending, summary.revision), page: Math.floor(offset / messagePageSize), id: result.stable_message_id, shift: -(offset % messagePageSize) };
    setSelectedId(result.conversation_id);
    setMessagePage(Math.floor(offset / messagePageSize));
    setTargetMessageId(result.stable_message_id);
  };
  const openQuotedMessage = async (messageId: string, signal: AbortSignal) => {
    if (!token) return;
    const position = await getMessagePosition(token, messageId, messageSortAscending ? "asc" : "desc", signal);
    if (signal.aborted || currentPageRequest.current.context !== pageContext || currentPageRequest.current.page !== messagePage) return;
    if (position.conversation_id !== selectedId || !Number.isSafeInteger(position.offset_in_conversation) || position.offset_in_conversation < 0) {
      throw new Error("无法在当前会话中找到原消息。");
    }
    pendingPageAnchor.current = { context: pageContext, page: Math.floor(position.offset_in_conversation / messagePageSize), id: messageId, shift: -(position.offset_in_conversation % messagePageSize) };
    setMessagePage(Math.floor(position.offset_in_conversation / messagePageSize));
    setTargetMessageId(messageId);
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
      {section === "archive-health" ? <ArchiveHealthPage token={token} onOpenTasks={() => setSection("tasks")} /> : section === "tasks" ? <TaskCenterPage token={token} superAdminEnabled={superAdminEnabled} /> : section === "server-config" ? <ServerConfigPage token={token} onTokenChanged={setToken} localCollectionAvailable={summary.local_collection_available} collectingLocal={collectingLocal || Boolean(pendingCollection)} onOpenLocalCollection={openLocalCollectionSetup} /> : section === "settings" ? <SettingsPage token={token} superAdminEnabled={superAdminEnabled} onSuperAdminChange={setSuperAdminEnabled} /> : section === "local-export" ? <LocalExportPage token={token} onCompleted={(result) => showExportCompleted(result, "本机导出完成")} onFailed={(error) => setToast({ message: error instanceof Error ? error.message : "本机导出失败。", tone: "error" })} /> : <>
        <ConversationList token={token} conversations={conversations} paging={conversationPaging} selectedId={selectedId} messageSortAscending={messageSortAscending} superAdminEnabled={superAdminEnabled} collectedUsers={collectedUsers} onSelect={(id) => { selectedConversationOverride.current = conversations.find((item) => item.id === id); setTargetMessageId(undefined); setSelectedId(id); }} onOpenSearchResult={openSearchResult} onExport={() => setShowExport(true)} localCollectionAvailable={summary.local_collection_available} collectingLocal={collectingLocal || Boolean(pendingCollection)} onOpenLocalCollection={openLocalCollectionFromConversation} importing={importing || Boolean(pendingImport)} onImport={(file) => void importPackage(file)} />
        {superAdminEnabled ? <MessageTimeline token={token} conversation={selectedConversation} messages={messages} participants={participants} sortAscending={messageSortAscending} onSortChange={(ascending) => { setMessageSortAscending(ascending); setMessagePage(0); }} status={messageStatus} targetMessageId={targetMessageId} onTargetLocated={() => setTargetMessageId(undefined)} onLocateQuote={openQuotedMessage} pageIndex={messagePage} pageSize={messagePageSize} totalMessages={messageTotal} totalPages={Math.max(1, Math.ceil(messageTotal / messagePageSize))} onPageSizeChange={(size) => setMessagePageSize(size)} onPageChange={(page) => setMessagePage(page)} onPreviousPage={() => setMessagePage((current) => Math.max(0, current - 1))} onNextPage={() => setMessagePage((current) => current + 1)} /> : <main className="content-gate"><ShieldCheck size={28} /><h2>会话内容已隐藏</h2></main>}
        {superAdminEnabled && selectedConversation.isGroup && <DetailPanel conversation={selectedConversation} participants={participants} messages={messages} collapsed={detailCollapsed} onToggleCollapsed={() => setDetailCollapsed((current) => !current)} />}
      </>}
    </div>
    <footer className="status-bar"><span>已归档 {summary.message_count.toLocaleString("zh-CN")} 条消息 · {summary.media_count.toLocaleString("zh-CN")} 项媒体 · {summary.conversation_count.toLocaleString("zh-CN")} 个会话</span><span><i />受控访问 · 审计已启用</span></footer>
     {showExport && superAdminEnabled && <ExportDialog token={token} conversationPaging={conversationPaging} scope={scope} format={format} dataRedaction={exportDataRedaction} simplify={exportSimplify} pretty={exportPretty} conversationOrder={exportConversationOrder} messageOrder={exportMessageOrder} onConversationOrderChange={setExportConversationOrder} onMessageOrderChange={setExportMessageOrder} onSimplifyChange={setExportSimplify} onPrettyChange={setExportPretty} conversations={conversations} selectedConversationId={selectedId} onScopeChange={setScope} onFormatChange={setFormat} onDataRedactionChange={setExportDataRedaction} messageCount={scope === "entire_archive" || scope === "date_range" ? summary.message_count : scope === "current_filter" ? messages.length : scope === "selected_conversations" ? 0 : selectedConversation.messageCount} mediaCount={scope === "entire_archive" || scope === "date_range" ? summary.media_count : scope === "current_filter" ? messages.filter((message) => message.attachment).length : selectedConversation.mediaCount} onOpenDirectoryFailed={(error) => setToast({ message: error instanceof Error ? error.message : "无法打开导出目录。", tone: "error" })} onClose={() => setShowExport(false)} onConfirm={async (selection: ExportSelection, signal, onProgress, taskId) => { const result = await createServerExport(token, { scope, format, conversationId: selectedId || undefined, conversationIds: selection.conversationIds, startDate: selection.startDate, endDate: selection.endDate, dataRedaction: exportDataRedaction, simplify: exportSimplify, pretty: exportPretty, conversationOrder: exportConversationOrder, messageOrder: exportMessageOrder }, signal, onProgress, taskId); setShowExport(false); showExportCompleted(result, "服务端导出完成"); }} />}
    {showLocalCollectionSetup && !collectingLocal && <LocalCollectionSetupDialog dataRedaction={localCollectionDataRedaction} onDataRedactionChange={setLocalCollectionDataRedaction} onClose={() => setShowLocalCollectionSetup(false)} onCollect={(includeMedia) => { setShowLocalCollectionSetup(false); void collectLocal(includeMedia); }} />}
    {customRootIssue && !collectingLocal && <CustomSourceRootDialog message={customRootIssue.message} initialRoot={customRootIssue.attemptedRoot ?? ""} onClose={() => setCustomRootIssue(null)} onRetry={(root) => { void collectLocal(customRootIssue.includeMedia, root); }} />}
    {collectingLocal && <LocalCollectionDialog progress={collectionProgress} />}
    <div className="archive-task-status-list">
      {importProgress && <ArchiveTaskStatus progress={importProgress} uncertain={Boolean(pendingImport)} checking={importing && Boolean(pendingImport)} onRecheck={() => void recheckArchiveTask("import")} onDismiss={() => { setPendingImport(undefined); setImportProgress(undefined); }} />}
      {collectionTask && pendingCollection && !collectingLocal && <ArchiveTaskStatus progress={collectionTask} uncertain checking={false} onRecheck={() => void recheckArchiveTask("collection")} onDismiss={() => { setPendingCollection(undefined); setCollectionTask(undefined); }} />}
    </div>
    {toast && <Toast tone={toast.tone} actionLabel={toast.actionLabel} onAction={toast.onAction} onClose={() => setToast(undefined)}>{toast.message}</Toast>}
  </div>;
}

function mapConversation(row: ServerConversation, index: number): ConversationSummary {
  const explicitType = row.conversation_type?.trim().toLowerCase();
  // Source conversation prefixes are more reliable than a stale persisted type
  // from archives created before direct/group classification was explicit.
  const isGroup = row.conversation_id.startsWith("R:")
    ? true
    : row.conversation_id.startsWith("S:") || row.conversation_id.startsWith("M:") || row.conversation_id.startsWith("T:") || row.conversation_id.startsWith("Y:")
      ? false
      : explicitType === "group";
  const directNames = (row.participant_names || []).map((name) => name.trim()).filter(Boolean);
  const kind = explicitType === "document" ? "document" : explicitType === "system" || row.conversation_id.startsWith("Y:") ? "system" : explicitType === "assistant" || row.conversation_id.startsWith("O:") ? "assistant" : explicitType === "temporary" || row.conversation_id.startsWith("T:") ? "temporary" : row.conversation_id.startsWith("M:") || explicitType === "application" ? "application" : undefined;
  const title = !isGroup && !kind && directNames.length > 0
    ? directNames.join("、")
    : row.display_name?.trim() || (kind === "temporary" ? "临时会话" : kind === "assistant" ? "应用会话" : kind === "system" ? "系统通知" : `会话 ${shortIdentifier(row.conversation_id)}`);
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
    kind,
  };
}

function mapSearchConversation(result: GlobalSearchResult): ConversationSummary {
  const title = result.conversation_name?.trim() || (result.conversation_id.startsWith("T:") ? "临时会话" : result.conversation_id.startsWith("R:") ? "未命名群聊" : result.conversation_id.startsWith("Y:") ? "系统通知" : result.conversation_id.startsWith("O:") ? "应用会话" : `会话 ${shortIdentifier(result.conversation_id)}`);
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
    kind: result.conversation_id.startsWith("Y:") ? "system" : result.conversation_id.startsWith("T:") ? "temporary" : result.conversation_id.startsWith("M:") ? "application" : result.conversation_id.startsWith("O:") ? "assistant" : undefined,
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
    body: row.body_text
      || (row.message_type === "audio" ? (media ? undefined : "[语音消息]")
        : row.message_type === "unsupported" ? `[暂不支持的消息类型：${row.raw_type}]` : undefined),
    richSegments: row.raw_payload?.rich_segments,
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
    lifecycle: row.lifecycle === "recalled" ? "recalled" : "active",
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

function formatBytes(value?: number | null): string {
  if (value == null) return "";
  if (value < 1024) return `${value} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KB`;
  return `${(value / 1024 / 1024).toFixed(1)} MB`;
}
