import { useCallback, useEffect, useRef, useState } from "react";
import { CONVERSATION_PAGE_SIZE, listConversationPage, type ServerConversation } from "./server-api";

export function useConversationPages() {
  const [rows, setRows] = useState<ServerConversation[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const session = useRef({ token: "", offset: 0, hasMore: false, controller: undefined as AbortController | undefined });

  const reset = useCallback(async (token: string, signal?: AbortSignal) => {
    const previous = session.current;
    previous.controller?.abort();
    const controller = new AbortController();
    const abort = () => controller.abort();
    signal?.addEventListener("abort", abort, { once: true });
    if (signal?.aborted) controller.abort();
    const next: typeof session.current = { token, offset: 0, hasMore: false, controller };
    session.current = next;
    setLoading(true);
    setError("");
    try {
      const page = await listConversationPage(token, 0, controller.signal);
      if (controller.signal.aborted) throw new DOMException("Aborted", "AbortError");
      next.offset = page.length;
      next.hasMore = page.length === CONVERSATION_PAGE_SIZE;
      setRows(page);
      setHasMore(next.hasMore);
      return page;
    } catch (reason) {
      if (session.current === next) {
        // Keep the displayed page usable after a failed refresh. A fresh
        // session identity still prevents the aborted old page from appending.
        session.current = { ...previous, controller: undefined };
        setHasMore(previous.hasMore);
        setLoading(false);
        if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : "会话列表刷新失败，请重试。");
      }
      throw reason;
    } finally {
      signal?.removeEventListener("abort", abort);
      if (session.current === next) {
        next.controller = undefined;
        setLoading(false);
      }
    }
  }, []);

  const loadMore = useCallback(async (token?: string) => {
    const current = session.current;
    if (!current.token || !current.hasMore || current.controller) return;
    if (token) current.token = token;
    const controller = new AbortController();
    current.controller = controller;
    setLoading(true);
    setError("");
    try {
      const page = await listConversationPage(current.token, current.offset, controller.signal);
      if (controller.signal.aborted || session.current !== current) return;
      current.offset += page.length;
      current.hasMore = page.length === CONVERSATION_PAGE_SIZE;
      setRows((previous) => {
        const incoming = new Map(page.map((row) => [row.conversation_id, row]));
        const merged = previous.map((row) => incoming.get(row.conversation_id) ?? row);
        const known = new Set(previous.map((row) => row.conversation_id));
        return [...merged, ...page.filter((row) => !known.has(row.conversation_id))];
      });
      setHasMore(current.hasMore);
    } catch (reason) {
      if (!controller.signal.aborted && session.current === current) {
        setError(reason instanceof Error ? reason.message : "会话列表加载失败，请重试。");
      }
    } finally {
      if (session.current === current && current.controller === controller) {
        current.controller = undefined;
        setLoading(false);
      }
    }
  }, []);

  useEffect(() => () => session.current.controller?.abort(), []);
  return { rows, hasMore, loading, error, reset, loadMore };
}
