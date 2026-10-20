import { ChevronDown, ChevronLeft, ChevronRight, ChevronUp, Download, ExternalLink, FileText, Image, Megaphone, Mic2, MoreVertical, SlidersHorizontal, X } from "lucide-react";
import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";
import type { ConversationSummary, MessageItem, ParticipantItem } from "../domain/types";
import { conversationKindLabel } from "../domain/conversations";
import { getMediaObjectUrl, openMediaFile } from "../lib/server-api";
import { ParticipantCard } from "./ParticipantCard";

interface MessageTimelineProps {
  token: string;
  conversation: ConversationSummary;
  messages: MessageItem[];
  participants: ParticipantItem[];
  status?: string;
  pageIndex?: number;
  pageSize?: number;
  totalMessages?: number;
  totalPages?: number;
  onPageChange?: (pageIndex: number) => void;
  onPageSizeChange?: (pageSize: number) => void;
  onPreviousPage?: () => void;
  onNextPage?: () => void;
  targetMessageId?: string;
  onTargetLocated?: () => void;
  sortAscending?: boolean;
  onSortChange?: (ascending: boolean) => void;
}

export function MessageTimeline({
  token,
  conversation,
  messages,
  participants,
  status,
  pageIndex = 0,
  pageSize = 200,
  totalMessages = conversation.messageCount,
  totalPages = Math.max(1, Math.ceil((totalMessages || conversation.messageCount) / pageSize)),
  onPageChange,
  onPageSizeChange,
  onPreviousPage,
  onNextPage,
  targetMessageId,
  onTargetLocated,
  sortAscending = false,
  onSortChange,
}: MessageTimelineProps) {
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
    const visible = messages.filter((message) => !(message.direction === "system" && isOpaqueSystemIdentifier(message.body)));
    return !onSortChange && sortAscending ? [...visible].reverse() : visible;
  }, [messages, onSortChange, sortAscending]);
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
    const element = scrollRef.current;
    element?.scrollTo({ top: edge === "top" ? 0 : element.scrollHeight, behavior: "smooth" });
  };

  useEffect(() => {
    if (targetMessageId) return;
    const element = scrollRef.current;
    if (!element) return;
    if (typeof element.scrollTo === "function") element.scrollTo({ top: 0, behavior: "auto" });
    else element.scrollTop = 0;
  }, [pageIndex]);

  useEffect(() => {
    if (!targetMessageId) return;
    const target = Array.from(scrollRef.current?.querySelectorAll<HTMLElement>("[data-message-id]") || [])
      .find((item) => item.dataset.messageId === targetMessageId);
    if (!target) return;
    target.scrollIntoView({ behavior: "smooth", block: "center" });
    setHighlightedMessageId(targetMessageId);
    onTargetLocated?.();
    window.setTimeout(() => setHighlightedMessageId((current) => current === targetMessageId ? undefined : current), 2200);
  }, [ordered, onTargetLocated, targetMessageId]);

  return (
    <section className="timeline-pane" aria-label="消息时间线">
      <header className="timeline-header">
        <div className="timeline-heading"><h1>{conversation.title}</h1><em className="conversation-kind-badge">{conversationKindLabel(conversation)}</em></div>
        <div className="timeline-actions">
          <button type="button" onClick={() => onSortChange?.(!sortAscending)}><SlidersHorizontal size={15} />按时间{sortAscending ? "升序" : "降序"}</button>
          <button className="icon-button" type="button" aria-label="更多操作"><MoreVertical size={18} /></button>
        </div>
      </header>
      <div className="date-divider">{boundaryDate && <span>{sortAscending ? `较早消息在前 · ${boundaryDate}` : `最近消息 · ${boundaryDate}`}</span>}</div>
      <div className="message-scroll-frame">
        <div className="message-scroll" ref={scrollRef}>
          {ordered.length === 0 ? <div className="empty-state"><span>{status || "当前条件下没有消息"}</span></div> : ordered.map((message) => <MessageRow key={message.id} token={token} message={message} highlighted={highlightedMessageId === message.id} onShowParticipant={() => showParticipant(message)} onLocateQuote={locateMessage} />)}
        </div>
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
      {selectedParticipant && <ParticipantCard participant={participantsById.get(selectedParticipant.id) || selectedParticipant} onClose={() => setSelectedParticipant(undefined)} />}
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
    return <article className={highlighted ? "message-row system-message-row highlighted" : "message-row system-message-row"} data-message-id={message.id}>
      <div className="message-content">
        <div className="message-meta"><span className="system-sender-name">{message.senderName || "系统"}</span><time className="message-time">{formatFullTimestamp(message.sentAt)}</time></div>
        <div className={announcement ? "message-bubble announcement-message" : "message-bubble"}>{announcement && <Megaphone size={14} />}<RichMessageText body={message.body || "系统消息"} segments={message.richSegments} /></div>
      </div>
    </article>;
  }
  const body = message.body?.trim();
  const attachmentOnly = Boolean(
    message.attachment
      && !message.quote
      && (message.type === "image" || message.type === "file")
      && (!body || body === message.attachment.name.trim()),
  );
  return (
    <article className={highlighted ? "message-row highlighted" : "message-row"} data-message-id={message.id}>
      <div className="message-content">
        <div className="message-meta"><button className="sender-name" type="button" onClick={onShowParticipant}>{message.senderName}</button><time className="message-time">{formatFullTimestamp(message.sentAt)}</time></div>
        {attachmentOnly
          ? <div className="message-attachment-only"><Attachment token={token} attachment={message.attachment!} /></div>
          : <div className="message-bubble">
            {message.quote && <button className="quote-block" type="button" onClick={() => onLocateQuote(message.quote!.id)} title="定位到原消息"><span><strong>{message.quote.sender}</strong>{message.quote.time && <> · {message.quote.time}</>}</span><p>{message.quote.body}</p></button>}
            {message.body && <RichMessageText body={message.body} segments={message.richSegments} />}
            {message.attachment && <Attachment token={token} attachment={message.attachment} />}
          </div>}
      </div>
    </article>
  );
}

const knownWeComEmoji: Record<string, string> = {
  "便便": "💩",
  "微笑": "🙂",
  "撇嘴": "🙁",
  "色": "😍",
  "发呆": "😳",
  "得意": "😎",
  "流泪": "😢",
  "害羞": "😊",
  "闭嘴": "🤐",
  "睡": "😴",
  "大哭": "😭",
  "发怒": "😡",
  "调皮": "😜",
  "龇牙": "😁",
  "惊讶": "😲",
  "难过": "😞",
  "酷": "😎",
  "偷笑": "🤭",
  "可爱": "🥰",
  "白眼": "🙄",
  "鼓掌": "👏",
  "疑问": "❓",
  "再见": "👋",
  "强": "👍",
  "弱": "👎",
};

function RichMessageText({ body, segments }: { body: string; segments?: MessageItem["richSegments"] }) {
  const card = parseWeComCard(body);
  if (card) {
    return <div className="wecom-rich-card"><h3>{renderInline(card.title)}</h3><div className="wecom-card-lines">{card.lines.map((line, index) => <div className="wecom-card-line" key={`${line}-${index}`}>{renderInline(line)}</div>)}</div></div>;
  }
  if (segments?.length) {
    return <div className="message-rich-text">{segments.map((segment, index) => segment.kind === "emoji"
      ? <WeComEmoji key={`${segment.text}-${index}`} label={segment.text} />
      : <span key={`${segment.text}-${index}`}>{segment.text}</span>)}</div>;
  }
  const tokenPattern = /\[([^\]\r\n]{1,24})\]/g;
  const nodes: ReactNode[] = [];
  let cursor = 0;
  let match: RegExpExecArray | null;
  while ((match = tokenPattern.exec(body))) {
    if (match.index > cursor) nodes.push(body.slice(cursor, match.index));
    const token = match[1];
    nodes.push(knownWeComEmoji[token] ? <WeComEmoji key={`${match.index}-${token}`} label={token} /> : match[0]);
    cursor = match.index + match[0].length;
  }
  if (cursor < body.length) nodes.push(body.slice(cursor));
  return <div className="message-rich-text">{nodes}</div>;
}

function parseWeComCard(body: string): { title: string; lines: string[] } | undefined {
  const lines = body.replace(/\r\n?/g, "\n").split("\n");
  const title = lines[0]?.match(/^#{1,6}\s+(.+)$/)?.[1]?.trim();
  if (!title) return undefined;
  const nonEmpty = lines.slice(1).filter((line) => line.trim());
  const cardLines = nonEmpty.map((line) => line.match(/^>\s?(.*)$/)?.[1]);
  return cardLines.length > 0 && cardLines.every((line): line is string => line !== undefined)
    ? { title, lines: cardLines }
    : undefined;
}

function renderInline(value: string): ReactNode[] {
  const nodes: ReactNode[] = [];
  const pattern = /<font\b([^>]*)>([\s\S]*?)<\/font>|\[([^\]\r\n]{1,24})\]/gi;
  let cursor = 0;
  let match: RegExpExecArray | null;
  while ((match = pattern.exec(value))) {
    if (match.index > cursor) nodes.push(value.slice(cursor, match.index));
    if (match[1] !== undefined) {
      const color = match[1].match(/\bcolor=["'](info|warning|comment)["']/i)?.[1]?.toLowerCase();
      nodes.push(color
        ? <span className={`wecom-card-${color}`} key={`${match.index}-${color}`}>{renderInline(match[2])}</span>
        : <Fragment key={`${match.index}-font`}>{renderInline(match[2])}</Fragment>);
    } else nodes.push(knownWeComEmoji[match[3]] ? <WeComEmoji key={`${match.index}-${match[3]}`} label={match[3]} /> : match[0]);
    cursor = match.index + match[0].length;
  }
  if (cursor < value.length) nodes.push(value.slice(cursor));
  return nodes;
}

function WeComEmoji({ label }: { label: string }) {
  const normalized = label.replace(/^\[|\]$/g, "");
  const emoji = knownWeComEmoji[normalized];
  return <span className="wecom-emoji" role="img" aria-label={`企业微信表情：${normalized}`} title={`企业微信表情：${normalized}`}>{emoji || label}</span>;
}

function formatFullTimestamp(value: string): string {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  const pad = (part: number) => String(part).padStart(2, "0");
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

function Attachment({ token, attachment }: { token: string; attachment: NonNullable<MessageItem["attachment"]> }) {
  const [objectUrl, setObjectUrl] = useState<string>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string>();
  const [previewOpen, setPreviewOpen] = useState(false);
  const [confirmRun, setConfirmRun] = useState(false);
  const active = useRef(true);
  useEffect(() => { active.current = true; return () => { active.current = false; }; }, []);
  useEffect(() => {
    let cancelled = false;
    if (attachment.kind === "image" && attachment.contentHash) {
      setLoading(true);
      void getMediaObjectUrl(token, attachment.contentHash).then((url) => {
        if (cancelled) URL.revokeObjectURL(url);
        else setObjectUrl(url);
      }).catch(() => { if (!cancelled) setError("图片读取失败，请重新采集或重试。"); }).finally(() => { if (!cancelled) setLoading(false); });
    }
    return () => { cancelled = true; };
  }, [attachment.contentHash, attachment.kind, token]);
  useEffect(() => () => {
    if (objectUrl) URL.revokeObjectURL(objectUrl);
  }, [objectUrl]);
  const open = async (save = false) => {
    if (!attachment.contentHash || loading) return;
    setLoading(true);
    setError(undefined);
    try {
      if (attachment.kind === "image" || save) {
        const url = objectUrl || await getMediaObjectUrl(token, attachment.contentHash);
        if (!active.current) { if (!objectUrl) URL.revokeObjectURL(url); return; }
        if (!objectUrl) setObjectUrl(url);
        if (save) {
          const link = document.createElement("a");
          link.href = url;
          link.download = attachment.name;
          document.body.appendChild(link);
          link.click();
          link.remove();
        } else setPreviewOpen(true);
      } else {
        await openMediaFile(token, attachment.contentHash, attachment.name);
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "附件操作失败，请重试。");
    } finally {
      setLoading(false);
    }
  };
  if (attachment.kind === "image") {
    return <>
      <button className="image-preview" type="button" aria-label={`查看图片 ${attachment.name}`} onClick={() => void open()} disabled={!attachment.contentHash || loading}>
        {objectUrl ? <img src={objectUrl} alt={attachment.name} /> : <div className="mini-layout"><span /><span /><span /><span /></div>}
        <div><Image size={15} />{loading ? "正在读取…" : error || (!attachment.contentHash ? "未采集图片内容" : attachment.meta)}</div>
      </button>
      {previewOpen && objectUrl && <div className="media-preview-backdrop" role="presentation" onMouseDown={() => setPreviewOpen(false)}><section className="media-preview-dialog" role="dialog" aria-modal="true" aria-label={`图片预览 ${attachment.name}`} onMouseDown={(event) => event.stopPropagation()}><button type="button" aria-label="关闭图片预览" onClick={() => setPreviewOpen(false)}><X size={20} /></button><img src={objectUrl} alt={attachment.name} /><span>{attachment.name}</span></section></div>}
    </>;
  }
  if (attachment.kind === "audio") {
    return <div className="audio-attachment">
      <button className={error ? "audio-card unavailable" : "audio-card"} type="button" onClick={() => void open()} disabled={!attachment.contentHash || loading} aria-label="打开语音消息" title={attachment.contentHash ? "打开语音消息" : "未采集语音内容"}>
        <span className="audio-icon"><Mic2 size={18} /></span>
        <span><strong>语音消息</strong><small>{loading ? "正在处理…" : attachment.meta || "语音内容"}</small></span>
        <ExternalLink size={16} />
      </button>
      {attachment.contentHash
        ? <button className="attachment-save" type="button" disabled={loading} onClick={() => void open(true)} aria-label="保存语音消息"><Download size={13} />保存语音</button>
        : <p className="attachment-status" role="status">未采集语音内容。{messageAudioHint(attachment.name)}</p>}
      {error && <p className="attachment-status" role="alert">{error}</p>}
    </div>;
  }
  const executable = /\.(exe|msi|msp|com|bat|cmd|ps1|vbs|vbe|js|jse|wsf|wsh|scr|hta|lnk|reg|cpl)$/i.test(attachment.name);
  return <div className="file-attachment">
    <button className={error ? "file-card unavailable" : "file-card"} type="button" onClick={() => executable ? setConfirmRun(true) : void open()} disabled={!attachment.contentHash || loading} aria-label={`打开附件 ${attachment.name}`} title={attachment.contentHash ? `打开 ${attachment.name}` : "未采集附件内容"}>
      <span className="file-icon"><FileText size={20} /></span>
      <span><strong>{attachment.name}</strong><small>{loading ? "正在处理…" : attachment.meta}</small></span>
      <ExternalLink size={17} />
    </button>
    {attachment.contentHash
      ? <button className="attachment-save" type="button" disabled={loading} onClick={() => void open(true)} aria-label={`保存附件 ${attachment.name}`}><Download size={13} />保存附件</button>
      : <p className="attachment-status" role="status">未采集附件内容。请在企业微信中下载源文件，再使用“包含全内容”重新采集。</p>}
    {error && <p className="attachment-status" role="alert">{error}</p>}
    {confirmRun && <div className="modal-backdrop" role="presentation"><section className="modal-card" role="dialog" aria-modal="true" aria-label="确认运行附件" onKeyDown={(event) => { if (event.key === "Escape") setConfirmRun(false); }}><h2>确认运行附件</h2><p>即将运行 {attachment.name}，可能启动安装程序或修改系统。请确认文件来源可信。</p><div className="modal-actions"><button className="secondary-button" type="button" autoFocus onClick={() => setConfirmRun(false)}>取消</button><button className="primary-button" type="button" onClick={() => { setConfirmRun(false); void open(); }}>确认运行</button></div></section></div>}
  </div>;
}

function messageAudioHint(name: string): string {
  return name && name !== "audio 媒体" ? `源文件：${name}。` : "请在企业微信中下载源文件，再使用“包含全内容”重新采集。";
}
