import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp, Download, FileText, Image, LoaderCircle, Megaphone, MoreVertical, Search, SlidersHorizontal, UploadCloud } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { ConversationSummary, MessageItem, ParticipantItem } from "../domain/types";
import { getMediaObjectUrl } from "../lib/server-api";
import { ParticipantCard } from "./ParticipantCard";

interface MessageTimelineProps {
  token: string;
  conversation: ConversationSummary;
  messages: MessageItem[];
  participants: ParticipantItem[];
  onExport: () => void;
  localCollectionAvailable?: boolean;
  collectingLocal?: boolean;
  onCollectLocal?: () => void;
  status?: string;
  pageIndex?: number;
  pageSize?: number;
  totalMessages?: number;
  totalPages?: number;
  onPageChange?: (pageIndex: number) => void;
  onPageSizeChange?: (pageSize: number) => void;
  onPreviousPage?: () => void;
  onNextPage?: () => void;
}

export function MessageTimeline({
  token,
  conversation,
  messages,
  participants,
  status,
  onExport,
  localCollectionAvailable,
  collectingLocal,
  onCollectLocal,
  pageIndex = 0,
  pageSize = 200,
  totalMessages = conversation.messageCount,
  totalPages = Math.max(1, Math.ceil((totalMessages || conversation.messageCount) / pageSize)),
  onPageChange,
  onPageSizeChange,
  onPreviousPage,
  onNextPage,
}: MessageTimelineProps) {
  const [sortAscending, setSortAscending] = useState(true);
  const [messageSearch, setMessageSearch] = useState("");
  const [selectedParticipant, setSelectedParticipant] = useState<ParticipantItem>();
  const [highlightedMessageId, setHighlightedMessageId] = useState<string>();
  const [showTopJump, setShowTopJump] = useState(false);
  const [showBottomJump, setShowBottomJump] = useState(false);
  const [pageInput, setPageInput] = useState(String(pageIndex + 1));
  const scrollRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    setPageInput(String(Math.min(pageIndex + 1, totalPages)));
  }, [pageIndex, totalPages]);
  const ordered = useMemo(() => {
    const normalizedSearch = messageSearch.trim().toLocaleLowerCase("zh-CN");
    const filtered = messages.filter((message) => {
      if (message.direction === "system" && isOpaqueSystemIdentifier(message.body)) return false;
      return !normalizedSearch || `${message.senderName} ${message.body || ""} ${message.attachment?.name || ""}`.toLocaleLowerCase("zh-CN").includes(normalizedSearch);
    });
    return sortAscending ? [...filtered].reverse() : filtered;
  }, [messages, messageSearch, sortAscending]);
  const participantsById = useMemo(() => new Map(participants.map((participant) => [participant.id, participant])), [participants]);
  const boundaryDate = ordered[0]?.sentAt ? new Intl.DateTimeFormat("zh-CN", { year: "numeric", month: "2-digit", day: "2-digit" }).format(new Date(ordered[0].sentAt)) : "";

  const showParticipant = (message: MessageItem) => {
    setSelectedParticipant(participantsById.get(message.senderId) || { id: message.senderId, name: message.senderName });
  };

  const locateMessage = (messageId: string) => {
    const target = scrollRef.current?.querySelector<HTMLElement>(`[data-message-id="${CSS.escape(messageId)}"]`);
    if (!target) return;
    target.scrollIntoView({ behavior: "smooth", block: "center" });
    setHighlightedMessageId(messageId);
    window.setTimeout(() => setHighlightedMessageId((current) => current === messageId ? undefined : current), 1800);
  };

  useEffect(() => {
    const element = scrollRef.current;
    if (!element) return;
    const updateJumpButtons = () => {
      if (!ordered.length) {
        setShowTopJump(false);
        setShowBottomJump(false);
        return;
      }
      setShowTopJump(element.scrollTop > 56);
      setShowBottomJump(element.scrollHeight - element.scrollTop - element.clientHeight > 56);
    };
    updateJumpButtons();
    element.addEventListener("scroll", updateJumpButtons, { passive: true });
    return () => element.removeEventListener("scroll", updateJumpButtons);
  }, [ordered.length]);

  const scrollToEdge = (edge: "top" | "bottom") => {
    const items = scrollRef.current?.querySelectorAll<HTMLElement>("[data-message-id]");
    const target = items?.[edge === "top" ? 0 : items.length - 1];
    target?.scrollIntoView({ behavior: "smooth", block: "nearest" });
  };

  return (
    <section className="timeline-pane" aria-label="消息时间线">
      <header className="timeline-header">
        <div className="timeline-heading"><h1>{conversation.title}</h1><em className="conversation-kind-badge">{conversation.isGroup ? "群会话" : "私人会话"}</em><label className="message-search"><Search size={16} /><input value={messageSearch} onChange={(event) => setMessageSearch(event.target.value)} placeholder="搜索当前会话消息…" /></label></div>
        <div className="timeline-actions">
          {localCollectionAvailable && <button className="timeline-collect-button" type="button" disabled={collectingLocal} onClick={onCollectLocal}>{collectingLocal ? <LoaderCircle className="spin" size={15} /> : <UploadCloud size={15} />}{collectingLocal ? "采集中…" : "采集本机"}</button>}
          <button type="button" onClick={() => setSortAscending((value) => !value)}><SlidersHorizontal size={15} />按时间{sortAscending ? "升序" : "降序"}</button>
          <button className="timeline-export-button" type="button" onClick={onExport}><Download size={15} />导出</button>
          <button className="icon-button" type="button" aria-label="更多操作"><MoreVertical size={18} /></button>
        </div>
      </header>
      <div className="date-divider">{boundaryDate && <span>{sortAscending ? `较早消息在前 · ${boundaryDate}` : `最近消息 · ${boundaryDate}`}</span>}</div>
      <div className="message-scroll" ref={scrollRef}>
        {ordered.length === 0 ? <div className="empty-state"><span>{status || "当前条件下没有消息"}</span>{localCollectionAvailable && !conversation.id && <button className="primary-button" type="button" disabled={collectingLocal} onClick={onCollectLocal}><UploadCloud size={17} />采集本机企业微信</button>}</div> : ordered.map((message) => <MessageRow key={message.id} token={token} message={message} highlighted={highlightedMessageId === message.id} onShowParticipant={() => showParticipant(message)} onLocateQuote={locateMessage} />)}
        {showTopJump && <button className="latest-position-button at-top" type="button" onClick={() => scrollToEdge("top")} aria-label="到最上面" title="到最上面"><ChevronUp size={17} />到最上面</button>}
        {showBottomJump && <button className="latest-position-button" type="button" onClick={() => scrollToEdge("bottom")} aria-label="到最下面" title="到最下面"><ChevronDown size={17} />到最下面</button>}
      </div>
      <footer className="timeline-footer">
        <span className="pagination-total">共 {totalMessages.toLocaleString("zh-CN")} 条消息</span>
        <div className="pagination-controls">
          <label className="page-size-select">每页
            <select aria-label="每页数量" value={pageSize} onChange={(event) => onPageSizeChange?.(Number(event.target.value))}>
              {[100, 200, 500].map((size) => <option key={size} value={size}>{size}</option>)}
            </select>条
          </label>
          <button type="button" aria-label="上一页" disabled={pageIndex === 0} onClick={onPreviousPage}><ChevronLeft size={17} /></button>
          <span className="page-jump">第 <input aria-label="跳转页码" inputMode="numeric" min={1} max={totalPages} value={pageInput} onChange={(event) => setPageInput(event.target.value.replace(/\D/g, ""))} onBlur={() => commitPageInput(pageInput, totalPages, onPageChange)} onKeyDown={(event) => { if (event.key === "Enter") { commitPageInput(pageInput, totalPages, onPageChange); event.currentTarget.blur(); } }} /> / {totalPages} 页</span>
          <button type="button" aria-label="下一页" disabled={pageIndex >= totalPages - 1} onClick={onNextPage}><ChevronRight size={17} /></button>
        </div>
      </footer>
      {selectedParticipant && <ParticipantCard participant={selectedParticipant} onClose={() => setSelectedParticipant(undefined)} />}
    </section>
  );
}

function commitPageInput(value: string, totalPages: number, onPageChange?: (pageIndex: number) => void): void {
  const parsed = Number.parseInt(value, 10);
  const page = Number.isFinite(parsed) ? Math.min(Math.max(parsed, 1), totalPages) : 1;
  onPageChange?.(page - 1);
}

function isOpaqueSystemIdentifier(value?: string): boolean {
  const compact = value?.trim() || "";
  return /^\d{8,}$/.test(compact) || /^[0-9a-f-]{24,}$/i.test(compact);
}

function MessageRow({ token, message, highlighted, onShowParticipant, onLocateQuote }: { token: string; message: MessageItem; highlighted: boolean; onShowParticipant: () => void; onLocateQuote: (messageId: string) => void }) {
  if (message.direction === "system") {
    const announcement = message.rawType === "group_announcement" || message.body?.includes("公告");
    return <div className={announcement ? "system-message announcement-message" : "system-message"} data-message-id={message.id}><time>{message.timeLabel}</time><span>{announcement && <Megaphone size={14} />}{message.body || "系统消息"}</span></div>;
  }
  return (
    <article className={highlighted ? "message-row highlighted" : "message-row"} data-message-id={message.id}>
      <time className="message-time">{message.timeLabel}</time>
      <button className="sender-avatar" type="button" title={`查看 ${message.senderName} 的资料`} onClick={onShowParticipant}>{message.senderInitial}</button>
      <div className="message-content">
        <button className="sender-name" type="button" onClick={onShowParticipant}>{message.senderName}</button>
        <div className="message-bubble">
          {message.quote && <button className="quote-block" type="button" onClick={() => onLocateQuote(message.quote!.id)} title="定位到原消息"><span><strong>{message.quote.sender}</strong>{message.quote.time && <> · {message.quote.time}</>}</span><p>{message.quote.body}</p></button>}
          {message.body && <p>{message.body}</p>}
          {message.attachment && <Attachment token={token} attachment={message.attachment} />}
        </div>
      </div>
    </article>
  );
}

function Attachment({ token, attachment }: { token: string; attachment: NonNullable<MessageItem["attachment"]> }) {
  const [objectUrl, setObjectUrl] = useState<string>();
  const [loading, setLoading] = useState(false);
  const [unavailable, setUnavailable] = useState(false);
  useEffect(() => {
    let cancelled = false;
    if (attachment.kind === "image" && attachment.contentHash) {
      setLoading(true);
      void getMediaObjectUrl(token, attachment.contentHash).then((url) => {
        if (cancelled) URL.revokeObjectURL(url);
        else setObjectUrl(url);
      }).catch(() => { if (!cancelled) setUnavailable(true); }).finally(() => { if (!cancelled) setLoading(false); });
    }
    return () => { cancelled = true; };
  }, [attachment.contentHash, attachment.kind, token]);
  useEffect(() => () => {
    if (objectUrl) URL.revokeObjectURL(objectUrl);
  }, [objectUrl]);
  const open = async () => {
    if (!attachment.contentHash || loading) return;
    setLoading(true);
    try {
      const url = objectUrl || await getMediaObjectUrl(token, attachment.contentHash);
      if (!objectUrl) setObjectUrl(url);
      window.open(url, "_blank", "noopener,noreferrer");
    } finally {
      setLoading(false);
    }
  };
  if (attachment.kind === "image") {
    return <button className="image-preview" type="button" aria-label={`查看图片 ${attachment.name}`} onClick={() => void open()} disabled={!attachment.contentHash || loading || unavailable}>
      {objectUrl ? <img src={objectUrl} alt={attachment.name} /> : <div className="mini-layout"><span /><span /><span /><span /></div>}
      <div><Image size={15} />{loading ? "正在读取…" : unavailable ? "未上传图片内容" : attachment.meta}</div>
    </button>;
  }
  return (
    <div className="file-card">
      <span className="file-icon"><FileText size={20} /></span>
      <span><strong>{attachment.name}</strong><small>{attachment.meta}</small></span>
      <button type="button" aria-label="查看附件" title="查看附件" onClick={() => void open()} disabled={!attachment.contentHash || loading}><Download size={17} /></button>
    </div>
  );
}
