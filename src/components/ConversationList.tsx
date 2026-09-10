import { CalendarDays, ChevronDown, Search, UsersRound } from "lucide-react";
import { useDeferredValue, useMemo } from "react";
import type { ConversationSummary, FilterState, MessageType } from "../domain/types";

interface ConversationListProps {
  conversations: ConversationSummary[];
  selectedId: string;
  filters: FilterState;
  participantOptions: Array<{ id: string; label: string }>;
  onFiltersChange: (filters: FilterState) => void;
  onSelect: (id: string) => void;
}

export function ConversationList({ conversations, selectedId, filters, participantOptions, onFiltersChange, onSelect }: ConversationListProps) {
  const deferredSearch = useDeferredValue(filters.search.trim().toLowerCase());
  const visible = useMemo(
    () => conversations.filter((conversation) =>
      !deferredSearch || `${conversation.title} ${conversation.lastMessage}`.toLowerCase().includes(deferredSearch)),
    [conversations, deferredSearch],
  );

  const update = <K extends keyof FilterState>(key: K, value: FilterState[K]) => {
    onFiltersChange({ ...filters, [key]: value });
  };

  return (
    <aside className="conversation-pane" aria-label="会话列表">
      <div className="search-box">
        <Search size={17} />
        <input
          aria-label="搜索会话或消息"
          onChange={(event) => update("search", event.target.value)}
          placeholder="搜索会话或消息"
          value={filters.search}
        />
        <kbd>Ctrl F</kbd>
      </div>
      <div className="filter-row">
        <FilterSelect icon={<CalendarDays size={14} />} label="日期" value={filters.date} onChange={(value) => update("date", value)}>
          <option value="all">全部日期</option><option value="today">今天</option><option value="week">近 7 天</option>
        </FilterSelect>
        <FilterSelect icon={<UsersRound size={14} />} label="参与人" value={filters.participant} onChange={(value) => update("participant", value)}>
          <option value="all">全部参与人</option>{participantOptions.map((participant) => <option key={participant.id} value={participant.id}>{participant.label}</option>)}
        </FilterSelect>
        <FilterSelect label="类型" value={filters.type} onChange={(value) => update("type", value as MessageType | "all")}>
          <option value="all">全部类型</option><option value="text">文本</option><option value="image">图片</option><option value="file">文件</option>
        </FilterSelect>
        <label className="media-filter"><input type="checkbox" checked={filters.mediaOnly} onChange={(event) => update("mediaOnly", event.target.checked)} /><span />仅媒体</label>
      </div>
      <div className="conversation-scroll">
        {visible.length === 0 ? <div className="conversation-empty">尚无归档数据，请使用企业版专属采集端。</div> : visible.map((conversation) => (
          <button
            aria-current={selectedId === conversation.id ? "true" : undefined}
            className={selectedId === conversation.id ? "conversation-item selected" : "conversation-item"}
            key={conversation.id}
            onClick={() => onSelect(conversation.id)}
            type="button"
          >
            <span className={`conversation-avatar ${conversation.accent}`}>{conversation.initials}</span>
            <span className="conversation-copy">
              <span className="conversation-title-row"><strong>{conversation.title}</strong><time>{conversation.lastAt}</time></span>
              <span className="conversation-last">{conversation.lastMessage}</span>
            </span>
            {conversation.unread > 0 && <span className="unread-count">{conversation.unread}</span>}
          </button>
        ))}
      </div>
      <div className="conversation-footer">共 {visible.length} 个会话</div>
    </aside>
  );
}

interface FilterSelectProps {
  children: React.ReactNode;
  icon?: React.ReactNode;
  label: string;
  value: string;
  onChange: (value: string) => void;
}

function FilterSelect({ children, icon, label, value, onChange }: FilterSelectProps) {
  return (
    <label className="filter-select" title={label}>
      {icon}<select aria-label={label} value={value} onChange={(event) => onChange(event.target.value)}>{children}</select><ChevronDown size={13} />
    </label>
  );
}
