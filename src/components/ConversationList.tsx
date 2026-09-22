import { ArrowRight, Download, HardDriveDownload, LoaderCircle, MessageSquareText, Search, Upload, Users, X } from "lucide-react";
import { useDeferredValue, useEffect, useMemo, useState } from "react";
import type { ConversationSummary } from "../domain/types";
import { searchMessages, type CollectedUser, type GlobalSearchResult } from "../lib/server-api";

interface ConversationListProps {
  token: string;
  conversations: ConversationSummary[];
  selectedId: string;
  messageSortAscending?: boolean;
  superAdminEnabled?: boolean;
  collectedUsers?: CollectedUser[];
  onSelect: (id: string) => void;
  onOpenSearchResult: (result: GlobalSearchResult) => void;
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
  const [searchTab, setSearchTab] = useState<"all" | "messages">("all");
  const [showSearchMenu, setShowSearchMenu] = useState(false);
  const deferredSearch = useDeferredValue(search.trim());
  const [results, setResults] = useState<GlobalSearchResult[]>([]);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState("");
  const [showCollectedUsers, setShowCollectedUsers] = useState(false);

  useEffect(() => {
    const openFromShortcut = (event: KeyboardEvent) => {
      if (superAdminEnabled && event.ctrlKey && event.altKey && event.key.toLocaleLowerCase() === "f") {
        event.preventDefault();
        setShowSearchMenu(false);
        setSearchOpen(true);
      }
      if (event.key === "Escape") {
        setSearchOpen(false);
        setShowSearchMenu(false);
        setSearch("");
      }
    };
    window.addEventListener("keydown", openFromShortcut);
    return () => window.removeEventListener("keydown", openFromShortcut);
  }, [superAdminEnabled]);

  useEffect(() => {
    if (!superAdminEnabled || !searchOpen || !deferredSearch) {
      setResults([]);
      setSearching(false);
      setSearchError("");
      return;
    }
    const controller = new AbortController();
    setSearching(true);
    setSearchError("");
    searchMessages(token, deferredSearch, controller.signal, messageSortAscending ? "asc" : "desc")
      .then(setResults)
      .catch((error) => {
        if (error instanceof DOMException && error.name === "AbortError") return;
        setResults([]);
        setSearchError(error instanceof Error ? error.message : "搜索失败");
      })
      .finally(() => {
        if (!controller.signal.aborted) setSearching(false);
      });
    return () => controller.abort();
  }, [deferredSearch, messageSortAscending, searchOpen, superAdminEnabled, token]);

  const conversationNames = useMemo(
    () => new Map(conversations.map((conversation) => [conversation.id, conversation.title])),
    [conversations],
  );
  const closeSearch = () => {
    setSearchOpen(false);
    setShowSearchMenu(false);
    setSearch("");
  };
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
              <span className="conversation-title-row"><strong>{conversation.title}</strong><em className="conversation-kind-badge">{conversation.isGroup ? "群会话" : "私人会话"}</em><time>{conversation.lastAt}</time></span>
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
        <nav className="global-search-tabs" aria-label="搜索类别">
          <button className={searchTab === "all" ? "active" : undefined} type="button" onClick={() => setSearchTab("all")}>全部</button>
          <button className={searchTab === "messages" ? "active" : undefined} type="button" onClick={() => setSearchTab("messages")}>聊天记录</button>
        </nav>
        <div className="global-search-content">
          {!deferredSearch ? <div className="global-search-empty"><span><Search size={20} /></span><strong>搜索全部归档</strong><p>输入名称、发送人或消息内容开始搜索</p></div>
            : searching ? <div className="search-state"><LoaderCircle className="spin" size={18} />正在搜索…</div>
              : searchError ? <div className="search-state error">{searchError}</div>
                : results.length === 0 ? <div className="global-search-empty"><span><Search size={20} /></span><strong>没有找到相关记录</strong><p>请尝试更换关键词</p></div>
                  : <div className="global-search-result-group">
                    <div className="global-search-summary"><strong>聊天记录</strong><span>{results.length} 条结果</span></div>
                    {results.map((result) => (
                      <button
                        className="global-search-item"
                        key={result.stable_message_id}
                        type="button"
                        onClick={() => {
                          onOpenSearchResult(result);
                          closeSearch();
                        }}
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
                  </div>}
        </div>
        <footer className="global-search-footer"><span>↑↓ 选择</span><span>Enter 打开</span><span>Esc 关闭</span></footer>
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

function formatFullDateTime(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  const pad = (part: number) => String(part).padStart(2, "0");
  return date.getFullYear() + "-" + pad(date.getMonth() + 1) + "-" + pad(date.getDate()) + " " + pad(date.getHours()) + ":" + pad(date.getMinutes()) + ":" + pad(date.getSeconds());
}

function messageTypeLabel(type: GlobalSearchResult["message_type"]): string {
  return ({ text: "文本", image: "图片", audio: "语音", video: "视频", file: "文件", link: "链接", reply: "引用消息", system: "系统消息", unsupported: "暂不支持的消息" } as const)[type];
}
