import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { ConversationSummary } from "../domain/types";
import { ConversationList } from "./ConversationList";

const conversations: ConversationSummary[] = [{
  id: "conversation-1",
  title: "项目讨论",
  initials: "项目",
  accent: "blue",
  lastMessage: "已归档 2 条消息",
  lastAt: "09-15",
  unread: 0,
  messageCount: 2,
  mediaCount: 0,
  participantCount: 2,
  isGroup: true,
}];

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("global message search", () => {
  it("searches the complete archive and opens a selected message result", async () => {
    const result = {
      stable_message_id: "message-42",
      conversation_id: "conversation-1",
      sender_id: "participant-1",
      sender_name: "成员一",
      sent_at: "2026-09-15T10:20:30Z",
      direction: "incoming",
      message_type: "text",
      body_text: "需要定位的记录",
      lifecycle: "active",
      media: [],
      raw_type: "text",
      offset_in_conversation: 401,
    };
    const fetchMock = vi.fn().mockResolvedValue(new Response(JSON.stringify([result]), {
      status: 200,
      headers: { "Content-Type": "application/json" },
    }));
    vi.stubGlobal("fetch", fetchMock);
    const onOpenSearchResult = vi.fn();
    const onCollectLocal = vi.fn();

    const { container } = render(
      <ConversationList
        token="test-token"
        conversations={conversations}
        selectedId="conversation-1"
        onSelect={vi.fn()}
        onOpenSearchResult={onOpenSearchResult}
        onExport={vi.fn()}
        localCollectionAvailable
        collectingLocal={false}
        onCollectLocal={onCollectLocal}
        importing={false}
        onImport={vi.fn()}
      />,
    );

    fireEvent.focus(screen.getByRole("textbox", { name: "打开全局搜索" }));
    fireEvent.click(screen.getByRole("button", { name: /全局搜索/ }));
    fireEvent.change(screen.getByRole("textbox", { name: "全局搜索聊天记录" }), { target: { value: "定位" } });
    expect(await screen.findByText("需要定位的记录")).toBeInTheDocument();
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringContaining("/api/v1/search/messages?"),
      expect.objectContaining({ signal: expect.any(AbortSignal) }),
    );
    fireEvent.click(screen.getByText("需要定位的记录"));
    expect(onOpenSearchResult).toHaveBeenCalledWith(expect.objectContaining({
      stable_message_id: "message-42",
      offset_in_conversation: 401,
    }));
    expect(screen.queryByText("需要定位的记录")).not.toBeInTheDocument();
    const footer = container.querySelector(".conversation-footer");
    expect(footer).toHaveTextContent("共 1 个会话");
    expect(footer?.querySelector('[aria-label="导入会话"]')).not.toBeNull();
    expect([...footer!.querySelectorAll(".conversation-footer-action")].map((action) => action.textContent)).toEqual(["采集本机", "导入", "导出"]);
  });

  it("offers text-only and media collection modes before import", () => {
    const onCollectLocal = vi.fn();
    render(
      <ConversationList
        token="test-token"
        conversations={conversations}
        selectedId="conversation-1"
        onSelect={vi.fn()}
        onOpenSearchResult={vi.fn()}
        onExport={vi.fn()}
        localCollectionAvailable
        collectingLocal={false}
        onCollectLocal={onCollectLocal}
        importing={false}
        onImport={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "采集本机" }));
    fireEvent.click(screen.getByRole("menuitem", { name: /仅文本消息/ }));
    expect(onCollectLocal).toHaveBeenLastCalledWith(false);

    fireEvent.click(screen.getByRole("button", { name: "采集本机" }));
    fireEvent.click(screen.getByRole("menuitem", { name: /包含图片和文件/ }));
    expect(onCollectLocal).toHaveBeenLastCalledWith(true);
  });
});
