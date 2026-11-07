import { afterEach, vi } from "vitest";
import { createServerExport, getMediaObjectUrl, listConversations, listMessages, type ServerConversation } from "./server-api";

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

it("forwards media cancellation and avoids creating a URL after the body read was aborted", async () => {
  const controller = new AbortController();
  const blob = new Blob(["synthetic image"]);
  let resolve!: (value: Blob) => void;
  const mock = vi.fn(async () => ({ ok: true, blob: () => new Promise<Blob>((done) => { resolve = done; }) }));
  vi.stubGlobal("fetch", mock);
  const create = vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:unexpected");
  const pending = getMediaObjectUrl("synthetic-token", "synthetic-hash", controller.signal);
  await vi.waitFor(() => expect(resolve).toBeDefined());
  expect(mock).toHaveBeenCalledWith("/api/v1/media/synthetic-hash", { headers: { Authorization: "Bearer synthetic-token" }, signal: controller.signal });
  controller.abort();
  resolve(blob);
  await expect(pending).rejects.toMatchObject({ name: "AbortError" });
  expect(create).not.toHaveBeenCalled();
});

it("serializes revision-bound anchors while retaining the legacy offset request", async () => {
  const mock = vi.fn(async (_input: RequestInfo | URL) => Response.json([]));
  vi.stubGlobal("fetch", mock);
  await listMessages("token", { conversationId: "synthetic", limit: 100, offset: 9000 });
  let parameters = new URL(String(mock.mock.calls[0][0]), "http://localhost").searchParams;
  expect(parameters.get("offset")).toBe("9000");
  expect(parameters.has("anchor_message_id")).toBe(false);
  await listMessages("token", { conversationId: "synthetic", limit: 100, offset: 9000, anchorMessageId: "message & #?", anchorShift: 0, anchorRevision: 0 });
  parameters = new URL(String(mock.mock.calls[1][0]), "http://localhost").searchParams;
  expect(parameters.get("anchor_message_id")).toBe("message & #?");
  expect(parameters.get("anchor_shift")).toBe("0");
  expect(parameters.get("anchor_revision")).toBe("0");
  expect(parameters.get("offset")).toBe("9000");
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
