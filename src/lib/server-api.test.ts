import { afterEach, vi } from "vitest";
import { createServerExport, listConversations, type ServerConversation } from "./server-api";

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("waits for confirmed cleanup after cancellation and preserves a completed publication", async () => {
  const controller = new AbortController();
  let polls = 0;
  const statuses: string[] = [];
  const mock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.endsWith("/start")) return Response.json({ taskId: "synthetic-task" });
    if (url.endsWith("/cancel")) return Response.json({ status: "cancelling", stage: "清理中", percent: 20 });
    polls += 1;
    return Response.json({ status: polls === 1 ? "running" : "cancelled", stage: "测试阶段", percent: 20 });
  });
  vi.stubGlobal("fetch", mock);
  const promise = createServerExport("synthetic-token", { scope: "entire_archive", format: "json" }, controller.signal, (progress) => {
    statuses.push(progress.status);
    if (progress.status === "running") controller.abort();
  });
  await expect(promise).rejects.toMatchObject({ code: "EXPORT_CANCELLED" });
  expect(statuses).toEqual(["running", "cancelling", "cancelled"]);
  expect(mock.mock.calls.filter(([url]) => String(url).endsWith("/cancel"))).toHaveLength(1);
  expect(polls).toBe(2);
});

it("retains the task identity when cancellation happens before start responds", async () => {
  const controller = new AbortController();
  const completed = { exportId: "synthetic", fileName: "synthetic.json", messageCount: 1 };
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    if (String(input).endsWith("/start")) { controller.abort(); return Response.json({ taskId: "synthetic-task" }); }
    if (String(input).endsWith("/cancel")) return Response.json({ status: "completed", result: completed });
    return Response.json({ status: "completed", result: completed });
  }));
  await expect(createServerExport("synthetic-token", { scope: "entire_archive", format: "json" }, controller.signal)).resolves.toEqual(completed);
});

it("loads every conversation page beyond the server page limit", async () => {
  const rows = Array.from({ length: 201 }, (_, index): ServerConversation => ({
    conversation_id: `conversation-${index + 1}`,
    last_message_at: "2026-09-20T00:00:00Z",
    message_count: 1,
    media_count: 0,
    participant_count: 2,
  }));
  const fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = new URL(String(input), "http://localhost");
    const offset = Number(url.searchParams.get("offset") || 0);
    return Response.json(rows.slice(offset, offset + 200));
  });
  vi.stubGlobal("fetch", fetchMock);

  const result = await listConversations("token");

  expect(result).toHaveLength(201);
  expect(fetchMock).toHaveBeenCalledTimes(2);
  expect(fetchMock.mock.calls[1][0]).toContain("offset=200");
});
