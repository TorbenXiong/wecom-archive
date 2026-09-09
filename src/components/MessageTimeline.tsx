import { ChevronLeft, ChevronRight, Download, FileText, Image, MoreVertical, Search, SlidersHorizontal } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import type { ConversationSummary, MessageItem } from "../domain/types";

interface MessageTimelineProps {
  conversation: ConversationSummary;
  messages: MessageItem[];
  status?: string;
}

export function MessageTimeline({ conversation, messages, status }: MessageTimelineProps) {
  const [sortAscending, setSortAscending] = useState(true);
  const [messageSearch, setMessageSearch] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const ordered = useMemo(() => {
    const filtered = messages.filter((message) => !messageSearch || message.body?.includes(messageSearch));
    return sortAscending ? filtered : [...filtered].reverse();
  }, [messages, messageSearch, sortAscending]);

  return (
    <section className="timeline-pane" aria-label="消息时间线">
      <header className="timeline-header">
        <div><h1>{conversation.title}</h1><span>共 {conversation.messageCount.toLocaleString("zh-CN")} 条消息</span></div>
        <div className="timeline-actions">
          <button type="button" onClick={() => setSortAscending((value) => !value)}><SlidersHorizontal size={15} />按时间{sortAscending ? "升序" : "降序"}</button>
          <button className="icon-button" type="button" aria-label="更多操作"><MoreVertical size={18} /></button>
        </div>
      </header>
      <div className="date-divider"><span>2026-09-07</span></div>
      <div className="message-scroll" ref={scrollRef}>
        {ordered.length === 0 ? <div className="empty-state">{status || "当前条件下没有消息"}</div> : ordered.map((message) => <MessageRow key={message.id} message={message} />)}
      </div>
      <footer className="timeline-footer">
        <label className="message-search"><Search size={16} /><input value={messageSearch} onChange={(event) => setMessageSearch(event.target.value)} placeholder="搜索当前会话消息…" /></label>
        <div className="pagination"><button type="button" disabled><ChevronLeft size={17} /></button><span>已加载 {ordered.length} / {conversation.messageCount}</span><button type="button" disabled={ordered.length >= conversation.messageCount}><ChevronRight size={17} /></button></div>
      </footer>
    </section>
  );
}

function MessageRow({ message }: { message: MessageItem }) {
  if (message.direction === "system") {
    return <div className="system-message"><time>{message.timeLabel}</time><span>{message.body}</span></div>;
  }
  return (
    <article className="message-row">
      <time className="message-time">{message.timeLabel}</time>
      <span className="sender-avatar">{message.senderInitial}</span>
      <div className="message-content">
        <span className="sender-name">{message.senderName}</span>
        <div className="message-bubble">
          {message.quote && <div className="quote-block"><span>回复 <strong>{message.quote.sender}</strong> · {message.quote.time}</span><p>{message.quote.body}</p></div>}
          {message.body && <p>{message.body}</p>}
          {message.attachment && <Attachment attachment={message.attachment} />}
        </div>
      </div>
    </article>
  );
}

function Attachment({ attachment }: { attachment: NonNullable<MessageItem["attachment"]> }) {
  if (attachment.kind === "image") {
    return <div className="image-preview" role="img" aria-label={attachment.name}><div className="mini-layout"><span /><span /><span /><span /></div><div><Image size={15} />{attachment.meta}</div></div>;
  }
  return (
    <div className="file-card">
      <span className="file-icon"><FileText size={20} /></span>
      <span><strong>{attachment.name}</strong><small>{attachment.meta}</small></span>
      <button type="button" aria-label="导出附件"><Download size={17} /></button>
    </div>
  );
}
