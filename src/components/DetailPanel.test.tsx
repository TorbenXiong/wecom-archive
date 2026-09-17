import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { ConversationSummary, MessageItem } from "../domain/types";
import { DetailPanel, extractGroupInfo } from "./DetailPanel";

function systemMessage(id: string, body: string, rawType?: string): MessageItem {
  return {
    id,
    conversationId: "R:group",
    senderId: "system",
    senderName: "系统",
    senderInitial: "系",
    sentAt: "2026-09-15T00:00:00Z",
    timeLabel: "08:00",
    direction: "system",
    type: "system",
    rawType,
    body,
    lifecycle: "active",
  };
}

describe("group detail metadata", () => {
  it("lists group member names without member avatars", () => {
    const conversation: ConversationSummary = {
      id: "R:group", title: "测试群", initials: "测试", accent: "blue", lastMessage: "", lastAt: "", unread: 0,
      messageCount: 0, mediaCount: 0, participantCount: 2, isGroup: true,
    };
    const { container } = render(<DetailPanel conversation={conversation} participants={[{ id: "1", name: "成员甲" }, { id: "2", name: "成员乙" }]} messages={[]} collapsed={false} onToggleCollapsed={vi.fn()} />);
    expect(screen.getByRole("button", { name: "成员甲" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "成员乙" })).toBeInTheDocument();
    expect(container.querySelector(".participant-strip")).toBeNull();
    expect(container.querySelector(".participant-name-list span")).toBeNull();
  });

  it("opens the complete member list when the group has more than twelve members", () => {
    const conversation: ConversationSummary = {
      id: "R:group", title: "大群", initials: "大群", accent: "blue", lastMessage: "", lastAt: "", unread: 0,
      messageCount: 0, mediaCount: 0, participantCount: 14, isGroup: true,
    };
    const participants = Array.from({ length: 14 }, (_, index) => ({ id: String(index + 1), name: `成员${index + 1}` }));
    render(<DetailPanel conversation={conversation} participants={participants} messages={[]} collapsed={false} onToggleCollapsed={vi.fn()} />);

    expect(screen.queryByRole("button", { name: "成员13" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "更多>>>" }));
    expect(screen.getByRole("dialog", { name: "全部群成员" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "成员13" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "成员14" })).toBeInTheDocument();
    fireEvent.change(screen.getByRole("textbox", { name: "检索群成员" }), { target: { value: "成员14" } });
    expect(screen.queryByRole("button", { name: "成员13" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "成员14" }));
    expect(screen.getByRole("dialog", { name: "成员14" })).toBeInTheDocument();
    expect(screen.getByRole("dialog", { name: "全部群成员" })).toBeInTheDocument();
  });

  it("does not turn opaque system identifiers into announcements or boards", () => {
    expect(extractGroupInfo([systemMessage("1", "1688856080881486")])).toEqual({
      announcement: undefined,
      board: undefined,
    });
  });

  it("shows a real multiline announcement and removes an adjacent identifier", () => {
    const text = "1688856080881486\n各位要坐班车的小伙伴们，大家好\n【坐车注意事项】：\n1.进群请及时更改群昵称";
    expect(extractGroupInfo([systemMessage("1", text)]).announcement).toBe(
      "各位要坐班车的小伙伴们，大家好\n【坐车注意事项】：\n1.进群请及时更改群昵称",
    );
  });

  it("only returns a board when a board message is explicitly identified", () => {
    expect(extractGroupInfo([systemMessage("1", "群看板：本周完成上线准备")])).toEqual({
      announcement: undefined,
      board: "本周完成上线准备",
    });
  });

  it("uses collected group metadata even when the announcement is short", () => {
    expect(extractGroupInfo([systemMessage("1", "明天放假", "group_announcement")]).announcement).toBe("明天放假");
  });
});
