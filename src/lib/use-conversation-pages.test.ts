import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { useConversationPages } from "./use-conversation-pages";

const page = (start: number, count: number) => Array.from({ length: count }, (_, index) => ({
  conversation_id: `synthetic-${start + index}`, last_message_at: "2026-11-01T00:00:00Z",
  message_count: 1, media_count: 0, participant_count: 0,
}));

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("loads only the first page, coalesces scroll requests, and keeps the cursor on retry", async () => {
  let finish!: (response: Response) => void;
  const fetchMock = vi.fn().mockResolvedValueOnce(Response.json(page(0, 50)))
    .mockImplementationOnce(() => new Promise<Response>((resolve) => { finish = resolve; }))
    .mockResolvedValueOnce(Response.json(page(50, 1)));
  vi.stubGlobal("fetch", fetchMock);
  const { result } = renderHook(() => useConversationPages());
  await act(async () => { await result.current.reset("synthetic-token"); });
  expect(fetchMock).toHaveBeenCalledTimes(1);
  expect(result.current.rows).toHaveLength(50);
  let loading!: Promise<void>;
  act(() => { loading = result.current.loadMore(); void result.current.loadMore(); });
  expect(fetchMock).toHaveBeenCalledTimes(2);
  await act(async () => { finish(Response.json({ message: "synthetic retry" }, { status: 503 })); await loading; });
  expect(result.current.error).toBe("synthetic retry");
  await act(async () => { await result.current.loadMore(); });
  expect(fetchMock.mock.calls[2][0]).toContain("offset=50");
  expect(result.current.rows).toHaveLength(51);
  expect(result.current.hasMore).toBe(false);
  expect(result.current.error).toBe("");
});

it("discards an old page after a revision reset even when transport ignores abort", async () => {
  let finish!: (response: Response) => void;
  const fetchMock = vi.fn().mockResolvedValueOnce(Response.json(page(0, 50)))
    .mockImplementationOnce(() => new Promise<Response>((resolve) => { finish = resolve; }))
    .mockResolvedValueOnce(Response.json(page(500, 1)));
  vi.stubGlobal("fetch", fetchMock);
  const { result } = renderHook(() => useConversationPages());
  await act(async () => { await result.current.reset("synthetic-token"); });
  let loading!: Promise<void>;
  act(() => { loading = result.current.loadMore(); });
  await act(async () => { await result.current.reset("synthetic-token"); });
  await act(async () => { finish(Response.json(page(50, 1))); await loading; });
  expect(result.current.rows.map((row) => row.conversation_id)).toEqual(["synthetic-500"]);
  expect((fetchMock.mock.calls[1][1] as RequestInit).signal?.aborted).toBe(true);
});

it("aborts pending page requests on unmount", async () => {
  const fetchMock = vi.fn().mockResolvedValueOnce(Response.json(page(0, 50)))
    .mockImplementationOnce(() => new Promise<Response>(() => undefined));
  vi.stubGlobal("fetch", fetchMock);
  const { result, unmount } = renderHook(() => useConversationPages());
  await act(async () => { await result.current.reset("synthetic-token"); });
  act(() => { void result.current.loadMore(); });
  await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
  unmount();
  expect((fetchMock.mock.calls[1][1] as RequestInit).signal?.aborted).toBe(true);
});

it("retains the cursor after a failed refresh and ignores the aborted previous page", async () => {
  let finish!: (response: Response) => void;
  const fetchMock = vi.fn().mockResolvedValueOnce(Response.json(page(0, 50)))
    .mockImplementationOnce(() => new Promise<Response>((resolve) => { finish = resolve; }))
    .mockResolvedValueOnce(Response.json({ message: "synthetic refresh failure" }, { status: 503 }))
    .mockResolvedValueOnce(Response.json(page(50, 1)));
  vi.stubGlobal("fetch", fetchMock);
  const { result } = renderHook(() => useConversationPages());
  await act(async () => { await result.current.reset("synthetic-token"); });
  let pending!: Promise<void>;
  act(() => { pending = result.current.loadMore(); });
  await act(async () => {
    await expect(result.current.reset("synthetic-token")).rejects.toThrow("synthetic refresh failure");
  });
  expect(result.current.rows).toHaveLength(50);
  expect(result.current.hasMore).toBe(true);
  expect(result.current.loading).toBe(false);
  expect(result.current.error).toBe("synthetic refresh failure");
  await act(async () => { finish(Response.json(page(900, 1))); await pending; });
  expect(result.current.rows).toHaveLength(50);
  await act(async () => { await result.current.loadMore(); });
  expect(fetchMock.mock.calls[3][0]).toContain("offset=50");
  expect(result.current.rows.map((row) => row.conversation_id)).toEqual(page(0, 51).map((row) => row.conversation_id));
  expect(result.current.error).toBe("");
  expect(result.current.hasMore).toBe(false);
});
