import { ArrowRight, Download, HardDriveDownload, LoaderCircle, MessageSquareText, Search, Upload, Users, X } from "lucide-react";
import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import type { ConversationSummary, MessageType } from "../domain/types";
import { conversationKindLabel } from "../domain/conversations";
import { listConversationParticipants, searchMessages, type CollectedUser, type GlobalSearchResult, type ServerParticipant } from "../lib/server-api";

interface ConversationListProps {
  token: string;
  conversations: ConversationSummary[];
  selectedId: string;
  messageSortAscending?: boolean;
  superAdminEnabled?: boolean;
  collectedUsers?: CollectedUser[];
  onSelect: (id: string) => void;
  onOpenSearchResult: (result: GlobalSearchResult, signal?: AbortSignal) => void | Promise<void>;
  onExport: () => void;
  localCollectionAvailable: boolean;
  collectingLocal: boolean;
  onOpenLocalCollection: () => void;
  importing: boolean;
  onImport: (file: File) => void;
}

export function ConversationList({
  token,
  conversations,
  selectedId,
  messageSortAscending = false,
  onSelect,
  onOpenSearchResult,
  onExport,
  localCollectionAvailable,
  collectingLocal,
  onOpenLocalCollection,
  importing,
  onImport,
  collectedUsers = [],
  superAdminEnabled = true,
}: ConversationListProps) {
  const [search, setSearch] = useState("");
  const [searchOpen, setSearchOpen] = useState(false);
  const [showSearchMenu, setShowSearchMenu] = useState(false);
  const deferredSearch = useDeferredValue(search.trim());
  const [results, setResults] = useState<GlobalSearchResult[]>([]);
  const [searchOffset, setSearchOffset] = useState(0);
  const [hasMoreResults, setHasMoreResults] = useState(false);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState("");
  const [searchConversation, setSearchConversation] = useState("");
  const [searchParticipant, setSearchParticipant] = useState("");
  const [searchType, setSearchType] = useState<MessageType | "">("");
  const [searchMediaOnly, setSearchMediaOnly] = useState(false);
  const [searchStartsAt, setSearchStartsAt] = useState("");
  const [searchEndsAt, setSearchEndsAt] = useState("");
  const [searchParticipants, setSearchParticipants] = useState<ServerParticipant[]>([]);
  const [participantError, setParticipantError] = useState("");
  const [locatingId, setLocatingId] = useState<string>();
  const [locateError, setLocateError] = useState("");
  const [retryVersion, setRetryVersion] = useState(0);
  const [showCollectedUsers, setShowCollectedUsers] = useState(false);
  const searchKey = useRef("");
  const locateController = useRef<AbortController | undefined>(undefined);
  const hasFilters = Boolean(searchConversation || searchParticipant || searchType || searchMediaOnly || searchStartsAt || searchEndsAt);
  const hasCriteria = Boolean(deferredSearch || hasFilters);

  useEffect(() => {
    setSearchParticipants([]);
    setParticipantError("");
    if (!searchOpen || !searchConversation) return;
    const controller = new AbortController();
    void listConversationParticipants(token, searchConversation, controller.signal).then((rows) => {
      if (!controller.signal.aborted) setSearchParticipants(rows);
    }).catch(() => {
      if (!controller.signal.aborted) setParticipantError("发送人列表读取失败，请重新选择会话。");
    });
    return () => controller.abort();
  }, [searchConversation, searchOpen, token]);

  useEffect(() => {
    const openFromShortcut = (event: KeyboardEvent) => {
      if (superAdminEnabled && event.ctrlKey && event.altKey && event.key.toLocaleLowerCase() === "f") {
        event.preventDefault();
        setShowSearchMenu(false);
        setSearchOpen(true);
      }
      if (event.key === "Escape") {
        locateController.current?.abort();
        setSearchOpen(false);
        setShowSearchMenu(false);
        setSearch("");
      }
    };
    window.addEventListener("keydown", openFromShortcut);
    return () => window.removeEventListener("keydown", openFromShortcut);
  }, [superAdminEnabled]);

  useEffect(() => {
    const key = `${token}\u0000${deferredSearch}\u0000${messageSortAscending ? "asc" : "desc"}\u0000${searchConversation}\u0000${searchParticipant}\u0000${searchType}\u0000${searchMediaOnly}\u0000${searchStartsAt}\u0000${searchEndsAt}`;
    const isNewSearch = searchKey.current !== key;
    searchKey.current = key;
    if (!superAdminEnabled || !searchOpen || !hasCriteria) {
      setResults([]);
      setSearchOffset(0);
      setHasMoreResults(false);
      setSearching(false);
      setSearchError("");
      return;
    }
    if (searchStartsAt && searchEndsAt && searchStartsAt > searchEndsAt) {
      setResults([]); setHasMoreResults(false); setSearching(false);
      setSearchError("开始日期不能晚于结束日期。");
      return;
    }
    if (isNewSearch && searchOffset !== 0) {
      setResults([]);
      setSearchOffset(0);
      setHasMoreResults(false);
      return;
    }
    const controller = new AbortController();
    setSearching(true);
    if (searchOffset === 0) {
      setResults([]);
      setHasMoreResults(false);
      setSearchError("");
    }
    const timer = window.setTimeout(() => { void searchMessages(token, deferredSearch, controller.signal, messageSortAscending ? "asc" : "desc", searchOffset, {
      conversationId: searchConversation || undefined,
      participantId: searchParticipant.trim() || undefined,
      messageType: searchType || undefined,
      mediaOnly: searchMediaOnly,
      startsAt: searchStartsAt ? localDateBoundary(searchStartsAt, false) : undefined,
      endsAt: searchEndsAt ? localDateBoundary(searchEndsAt, true) : undefined,
    })
      .then((next) => {
        if (controller.signal.aborted) return;
        setResults((current) => {
          if (searchOffset === 0) return next;
          const existing = new Set(current.map((item) => item.stable_message_id));
          return [...current, ...next.filter((item) => !existing.has(item.stable_message_id))];
        });
        setHasMoreResults(next.length === 50);
      })
      .catch((error) => {
        if (controller.signal.aborted || (error instanceof DOMException && error.name === "AbortError")) return;
        setSearchError(error instanceof Error ? error.message : "搜索失败");
      })
      .finally(() => {
        if (!controller.signal.aborted) setSearching(false);
      }); }, searchOffset === 0 ? 180 : 0);
    return () => { window.clearTimeout(timer); controller.abort(); };
  }, [deferredSearch, hasCriteria, messageSortAscending, retryVersion, searchConversation, searchEndsAt, searchMediaOnly, searchOffset, searchOpen, searchParticipant, searchStartsAt, searchType, superAdminEnabled, token]);

  const conversationNames = useMemo(
    () => new Map(conversations.map((conversation) => [conversation.id, conversation.title])),
    [conversations],
  );
  const closeSearch = () => {
    locateController.current?.abort();
    setSearchOpen(false);
    setShowSearchMenu(false);
    setSearch("");
    setLocateError(""); setLocatingId(undefined);
  };
  const resetFilters = () => {
    setSearchConversation(""); setSearchParticipant(""); setSearchType("");
    setSearchMediaOnly(false); setSearchStartsAt(""); setSearchEndsAt(""); setSearchOffset(0);
  };
  const openResult = (result: GlobalSearchResult) => {
    if (locatingId) return;
    setLocateError("");
    try {
      const controller = new AbortController();
      locateController.current = controller;
      const opened = onOpenSearchResult(result, controller.signal);
      if (opened instanceof Promise) {
        setLocatingId(result.stable_message_id);
        const timer = window.setTimeout(() => controller.abort(), 30000);
        void opened.then(() => { if (!controller.signal.aborted) closeSearch(); }).catch((error) => {
          if (controller.signal.aborted) setLocateError("定位请求已中止，请重试。");
          else setLocateError(error instanceof Error ? error.message : "无法定位消息，请重试。");
        }).finally(() => { window.clearTimeout(timer); setLocatingId(undefined); });
      } else closeSearch();
    } catch (error) { setLocateError(error instanceof Error ? error.message : "无法定位消息，请重试。"); }
  };
  useEffect(() => () => locateController.current?.abort(), []);
  const updateSearch = (value: string) => {
    setSearch(value);
    setShowSearchMenu(false);
    setSearchOpen(true);
  };

  return (
    <aside className="conversation-pane" aria-label="会话列表">
      <div className="conversation-toolbar" onBlur={(event) => {
        if (!event.currentTarget.contains(event.relatedTarget)) setShowSearchMenu(false);
      }}>
        {superAdminEnabled && <div className="search-box conversation-search-box">
          <Search size={17} />
          <input
            aria-label="打开全局搜索"
            onChange={(event) => updateSearch(event.target.value)}
            onFocus={() => setShowSearchMenu(true)}
            placeholder="搜索"
            value={searchOpen ? search : ""}
          />
        </div>}
        {superAdminEnabled && showSearchMenu && !searchOpen && <div className="search-mode-menu">
          <button type="button" onMouseDown={(event) => event.preventDefault()} onClick={() => { setShowSearchMenu(false); setSearchOpen(true); }}>
            <span className="search-mode-icon"><Search size={16} /></span>
            <span><strong>全局搜索</strong><small>搜索全部归档聊天记录</small></span>
            <kbd>Ctrl+Alt+F</kbd>
            <ArrowRight size={15} />
          </button>
        </div>}
      </div>

      <div className="conversation-scroll">
        <button className="collected-users-summary" type="button" onClick={() => setShowCollectedUsers(true)}><Users size={15} /><span>已收集 <strong>{collectedUsers.length.toLocaleString("zh-CN")}</strong> 位用户聊天记录</span></button>
        {conversations.length === 0 ? <div className="conversation-empty">尚无归档数据，可采集本机或使用专属采集端。</div> : conversations.map((conversation) => (
          <button
            aria-current={selectedId === conversation.id ? "true" : undefined}
            className={selectedId === conversation.id ? "conversation-item selected" : "conversation-item"}
            key={conversation.id}
            onClick={() => onSelect(conversation.id)}
            type="button"
          >
            <span className={"conversation-avatar " + conversation.accent}>{conversation.initials}</span>
            <span className="conversation-copy">
              <span className="conversation-title-row"><strong>{conversation.title}</strong><em className="conversation-kind-badge">{conversationKindLabel(conversation)}</em><time>{conversation.lastAt}</time></span>
              <span className="conversation-last">{conversation.lastMessage}</span>
            </span>
            {conversation.unread > 0 && <span className="unread-count">{conversation.unread}</span>}
          </button>
        ))}
      </div>

      <div className="conversation-footer">
        <span>共 {conversations.length} 个会话</span>
        <div className="conversation-footer-actions">
          {localCollectionAvailable && <div className="collection-action-wrap">
            <button className="conversation-footer-action" disabled={collectingLocal} onClick={onOpenLocalCollection} type="button">
              {collectingLocal ? <LoaderCircle className="spin" size={14} /> : <HardDriveDownload size={14} />}
              {collectingLocal ? "采集中…" : "采集本机"}
            </button>
          </div>}
          <label className={importing ? "conversation-footer-action disabled" : "conversation-footer-action"} title="支持采集端 .wca 加密包和本机导出的 JSON">
            <Upload size={14} /><span>{importing ? "导入中" : "导入"}</span>
            <input aria-label="导入会话" type="file" accept=".wca,.json" hidden disabled={importing} onChange={(event) => { const file = event.target.files?.[0]; if (file) onImport(file); event.currentTarget.value = ""; }} />
          </label>
          {superAdminEnabled && <button className="conversation-footer-action" type="button" onClick={onExport}><Download size={14} />导出</button>}
        </div>
      </div>

      {superAdminEnabled && searchOpen && <section className="global-search-overlay" role="dialog" aria-modal="true" aria-label="全局搜索">
        <header className="global-search-header">
          <label className="global-search-input">
            <Search size={19} />
            <input autoFocus aria-label="全局搜索聊天记录" value={search} onChange={(event) => setSearch(event.target.value)} placeholder="搜索全部归档聊天记录" />
            {search && <button type="button" onClick={() => setSearch("")}>清除</button>}
          </label>
          <button className="global-search-close" type="button" aria-label="关闭全局搜索" onClick={closeSearch}><X size={20} /></button>
        </header>
        <div className="global-search-filters" aria-label="搜索筛选">
          <label className="global-search-filter">会话<select aria-label="筛选会话" value={searchConversation} onChange={(event) => { setSearchConversation(event.target.value); setSearchParticipant(""); setSearchOffset(0); }}>
            <option value="">全部会话</option>
            {conversations.map((conversation) => <option key={conversation.id} value={conversation.id}>{conversation.title}</option>)}
          </select></label>
          <label className="global-search-filter">发送人<select aria-label="筛选发送人" value={searchParticipant} disabled={!searchConversation || searchParticipants.length === 0} onChange={(event) => { setSearchParticipant(event.target.value); setSearchOffset(0); }}>
            <option value="">{!searchConversation ? "先选择会话" : "全部发送人"}</option>
            {searchParticipants.map((participant, index) => <option key={participant.participant_id} value={participant.participant_id}>{participant.display_name?.trim() || `成员 ${index + 1}`}</option>)}
          </select></label>
          <label className="global-search-filter">消息类型<select aria-label="筛选消息类型" value={searchType} onChange={(event) => { setSearchType(event.target.value as MessageType | ""); setSearchOffset(0); }}>
            <option value="">全部类型</option><option value="text">文本</option><option value="image">图片</option><option value="file">文件</option><option value="audio">语音</option><option value="video">视频</option><option value="link">链接</option><option value="reply">引用</option><option value="system">系统</option><option value="unsupported">暂不支持</option>
          </select></label>
          <div className="global-search-filter date-filter"><span>日期范围</span><div className="global-search-date-range">
            <input aria-label="搜索开始日期" type="date" value={searchStartsAt} onChange={(event) => { setSearchStartsAt(event.target.value); setSearchOffset(0); }} /><span>至</span>
            <input aria-label="搜索结束日期" type="date" value={searchEndsAt} onChange={(event) => { setSearchEndsAt(event.target.value); setSearchOffset(0); }} />
          </div></div>
          <div className="global-search-filter-actions"><label><input type="checkbox" checked={searchMediaOnly} onChange={(event) => { setSearchMediaOnly(event.target.checked); setSearchOffset(0); }} />仅含附件</label><button type="button" className="global-search-reset" disabled={!hasFilters} onClick={resetFilters}>重置筛选</button></div>
        </div>
        <div className="global-search-content">
          {participantError && <div className="global-search-feedback error" role="alert">{participantError}</div>}
          {locatingId && <div className="global-search-feedback" role="status"><LoaderCircle className="spin" size={16} />正在定位原消息…</div>}
          {locateError && <div className="global-search-feedback error" role="alert">{locateError}</div>}
          {!hasCriteria ? <div className="global-search-empty"><span><Search size={20} /></span><strong>搜索全部归档</strong><p>输入消息内容、附件名称，或选择筛选条件</p></div>
            : searching && results.length === 0 ? <div className="search-state" role="status"><LoaderCircle className="spin" size={18} />正在搜索…</div>
              : searchError && results.length === 0 ? <div className="global-search-feedback error" role="alert">{searchError}<button type="button" onClick={() => setRetryVersion((value) => value + 1)}>重试</button></div>
                : results.length === 0 ? <div className="global-search-empty"><span><Search size={20} /></span><strong>没有找到相关记录</strong><p>请尝试更换关键词</p></div>
                  : <div className="global-search-result-group">
                    <div className="global-search-summary"><strong>聊天记录</strong><span>{results.length} 条结果{hasMoreResults ? "（可继续加载）" : ""}</span></div>
                    {results.map((result) => (
                      <button
                        className="global-search-item"
                        key={result.stable_message_id}
                        type="button"
                        disabled={Boolean(locatingId)}
                        onClick={() => openResult(result)}
                      >
                        <span className="global-search-record-icon"><MessageSquareText size={17} /></span>
                        <span className="global-search-record-copy">
                          <span className="global-search-item-head">
                            <strong>{conversationNames.get(result.conversation_id) || result.conversation_name?.trim() || "未命名会话"}</strong>
                            <time>{formatFullDateTime(result.sent_at)}</time>
                          </span>
                          <span className="global-search-sender">{result.sender_name?.trim() || "系统"}</span>
                          <span className="global-search-snippet">{result.body_text || result.media[0]?.original_name || "[" + messageTypeLabel(result.message_type) + "]"}</span>
                        </span>
                      </button>
                    ))}
                    {searchError && <div className="global-search-feedback error" role="alert">{searchError}<button type="button" onClick={() => setRetryVersion((value) => value + 1)}>重试</button></div>}
                    {hasMoreResults && <button className="global-search-load-more" type="button" disabled={searching} onClick={() => setSearchOffset((offset) => offset + 50)}>{searching ? "正在加载…" : "加载更多结果"}</button>}
                  </div>}
        </div>
        <footer className="global-search-footer"><span>点击结果查看原消息</span><span>Tab 选择 · Enter 打开</span><span>Esc 关闭</span></footer>
      </section>}
      {showCollectedUsers && <section className="collected-users-dialog-backdrop" role="presentation" onMouseDown={(event) => { if (event.target === event.currentTarget) setShowCollectedUsers(false); }}>
        <div className="collected-users-dialog" role="dialog" aria-modal="true" aria-label="已收集用户">
          <header><div><strong>已收集用户</strong><span>{collectedUsers.length} 位</span></div><button type="button" aria-label="关闭" onClick={() => setShowCollectedUsers(false)}><X size={18} /></button></header>
          <div className="collected-users-list">{collectedUsers.length === 0 ? <p>暂未采集到用户记录。</p> : collectedUsers.map((user, index) => { const fallbackName = `采集用户 ${index + 1}`; const name = user.display_name?.trim() || fallbackName; return <div className="collected-user-row" key={user.source_instance_id}><span className="conversation-avatar slate">{name.slice(0, 2)}</span><span><strong>{name}</strong></span></div>; })}</div>
        </div>
      </section>}
    </aside>
  );
}

function localDateBoundary(value: string, end: boolean): string {
  const [year, month, day] = value.split("-").map(Number);
  const date = end ? new Date(year, month - 1, day + 1) : new Date(year, month - 1, day);
  if (end) date.setTime(date.getTime() - 1);
  return date.toISOString();
}


function formatFullDateTime(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  const pad = (part: number) => String(part).padStart(2, "0");
  return date.getFullYear() + "-" + pad(date.getMonth() + 1) + "-" + pad(date.getDate()) + " " + pad(date.getHours()) + ":" + pad(date.getMinutes()) + ":" + pad(date.getSeconds());
}

function messageTypeLabel(type: GlobalSearchResult["message_type"]): string {
  return ({ text: "文本", image: "图片", audio: "语音", video: "视频", file: "文件", link: "链接", reply: "引用消息", system: "系统消息", unsupported: "暂不支持的消息" } as const)[type];
}
