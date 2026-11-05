export interface ConversationPaging {
  hasMore: boolean;
  loading: boolean;
  error: string;
  total: number;
  onLoadMore: () => void;
}

export function ConversationLoadMore({ paging, compact = false }: { paging?: ConversationPaging; compact?: boolean }) {
  if (!paging) return null;
  return <div className={compact ? "conversation-load-more compact" : "conversation-load-more"}>
    {paging.error && <p role="alert">{paging.error}</p>}
    {paging.hasMore && <button className="secondary-button" type="button" disabled={paging.loading} onClick={paging.onLoadMore}>
      {paging.loading ? "正在加载会话…" : paging.error ? "重试加载会话" : "加载更多会话"}
    </button>}
  </div>;
}
