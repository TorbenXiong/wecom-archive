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
    const onOpenLocalCollection = vi.fn();

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
        onOpenLocalCollection={onOpenLocalCollection}
        importing={false}
        onImport={vi.fn()}
      />,
    );

    fireEvent.focus(screen.getByRole("textbox", { name: "打开全局搜索" }));
    fireEvent.click(screen.getByRole("button", { name: /全局搜索/ }));
    fireEvent.change(screen.getByRole("textbox", { name: "全局搜索聊天记录" }), { target: { value: "定位" } });
    expect(await screen.findByText("需要定位的记录")).toBeInTheDocument();
    expect(fetchMock).toHaveBeenCalledWith(
      expect.stringMatching(/\/api\/v1\/search\/messages\?.*sort=desc/),
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

  it("opens collection configuration before starting local collection", () => {
    const onOpenLocalCollection = vi.fn();
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
        onOpenLocalCollection={onOpenLocalCollection}
        importing={false}
        onImport={vi.fn()}
      />,
    );

    fireEvent.click(screen.getByRole("button", { name: "采集本机" }));
    expect(onOpenLocalCollection).toHaveBeenCalledOnce();
  });

  it("keeps directory statistics visible while hiding content actions without super admin", () => {
    render(
      <ConversationList
        token="test-token"
        conversations={conversations}
        selectedId="conversation-1"
        superAdminEnabled={false}
        collectedUsers={[{ source_instance_id: "source-1", display_name: "成员一" }]}
        onSelect={vi.fn()}
        onOpenSearchResult={vi.fn()}
        onExport={vi.fn()}
        localCollectionAvailable={false}
        collectingLocal={false}
        onOpenLocalCollection={vi.fn()}
        importing={false}
        onImport={vi.fn()}
      />,
    );

    expect(screen.getByText("已归档 2 条消息")).toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "打开全局搜索" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "导出" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /已收集 1 位用户聊天记录/ }));
    expect(screen.getByRole("dialog", { name: "已收集用户" })).toHaveTextContent("成员一");
  });
});
