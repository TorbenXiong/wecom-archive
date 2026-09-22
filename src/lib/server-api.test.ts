import { afterEach, vi } from "vitest";
import { listConversations, type ServerConversation } from "./server-api";

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
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
