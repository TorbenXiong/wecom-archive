import { fireEvent, render, screen } from "@testing-library/react";
import { vi } from "vitest";
import { MessageTimeline } from "./MessageTimeline";
import type { ConversationSummary, MessageItem } from "../domain/types";

const conversation: ConversationSummary = {
  id: "conversation-1",
  title: "测试会话",
  initials: "测",
  accent: "blue",
  lastMessage: "较新消息",
  lastAt: "刚刚",
  unread: 0,
  messageCount: 2,
  mediaCount: 0,
  participantCount: 2,
  isGroup: true,
};

const messages: MessageItem[] = [
  {
    id: "newer-message",
    conversationId: conversation.id,
    senderId: "participant-1",
    senderName: "成员一",
    senderInitial: "一",
    sentAt: "2026-09-15T10:00:00Z",
    timeLabel: "18:00",
    direction: "incoming",
    type: "text",
    body: "较新消息",
    lifecycle: "active",
  },
  {
    id: "older-message",
    conversationId: conversation.id,
    senderId: "participant-2",
    senderName: "成员二",
    senderInitial: "二",
    sentAt: "2026-09-15T09:00:00Z",
    timeLabel: "17:00",
    direction: "incoming",
    type: "text",
    body: "较早消息",
    lifecycle: "active",
  },
];

describe("message timeline", () => {
  it("shows messages in chronological order and offers floating jumps to both edges", () => {
    const originalScrollIntoView = Element.prototype.scrollIntoView;
    const scrollIntoView = vi.fn();
    Element.prototype.scrollIntoView = scrollIntoView;
    try {
      const { container } = render(
        <MessageTimeline
          token="test-token"
          conversation={conversation}
          messages={messages}
          participants={[]}
          onExport={vi.fn()}
        />,
      );
      const scroll = container.querySelector<HTMLElement>(".message-scroll");
      expect(scroll).not.toBeNull();
      Object.defineProperty(scroll, "scrollHeight", { configurable: true, value: 800 });
      Object.defineProperty(scroll, "clientHeight", { configurable: true, value: 300 });
      Object.defineProperty(scroll, "scrollTop", { configurable: true, writable: true, value: 120 });
      fireEvent.scroll(scroll!);
      expect(
        [...container.querySelectorAll("[data-message-id]")].map((item) => item.getAttribute("data-message-id")),
      ).toEqual(["older-message", "newer-message"]);
      expect(screen.getByRole("button", { name: "按时间升序" })).toBeInTheDocument();

      expect(screen.getByRole("button", { name: "到最上面" })).toBeInTheDocument();
      const jumpButton = screen.getByRole("button", { name: "到最下面" });
      expect(jumpButton).toBeInTheDocument();
      fireEvent.click(jumpButton);
      expect(scrollIntoView).toHaveBeenCalledWith({ behavior: "smooth", block: "nearest" });
    } finally {
      Element.prototype.scrollIntoView = originalScrollIntoView;
    }
  });
});
