import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, vi } from "vitest";
import { MessageTimeline } from "./MessageTimeline";
import type { ConversationSummary, MessageItem } from "../domain/types";

const mediaApi = vi.hoisted(() => ({
  getMediaObjectUrl: vi.fn(() => new Promise<string>(() => undefined)),
  openMediaFile: vi.fn(async () => ({ opened: true })),
}));

vi.mock("../lib/server-api", () => mediaApi);

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
  beforeEach(() => {
    mediaApi.getMediaObjectUrl.mockClear();
    mediaApi.openMediaFile.mockClear();
  });

  it("shows messages in descending time order by default and offers floating jumps to both edges", () => {
    const { container } = render(
        <MessageTimeline
          token="test-token"
          conversation={conversation}
          messages={messages}
          participants={[]}
        />,
      );
      const scroll = container.querySelector<HTMLElement>(".message-scroll");
      expect(scroll).not.toBeNull();
      const scrollTo = vi.fn();
      Object.defineProperty(scroll, "scrollTo", { configurable: true, value: scrollTo });
      Object.defineProperty(scroll, "scrollHeight", { configurable: true, value: 800 });
      Object.defineProperty(scroll, "clientHeight", { configurable: true, value: 300 });
      Object.defineProperty(scroll, "scrollTop", { configurable: true, writable: true, value: 120 });
      fireEvent.scroll(scroll!);
      expect(
        [...container.querySelectorAll("[data-message-id]")].map((item) => item.getAttribute("data-message-id")),
      ).toEqual(["newer-message", "older-message"]);
      expect(screen.getByRole("button", { name: "按时间降序" })).toBeInTheDocument();

      expect(screen.getByRole("button", { name: "到最上面" })).toBeInTheDocument();
      const jumpButton = screen.getByRole("button", { name: "到最下面" });
      expect(jumpButton).toBeInTheDocument();
      fireEvent.click(jumpButton);
      expect(scrollTo).toHaveBeenCalledWith({ behavior: "smooth", top: 800 });
      fireEvent.click(screen.getByRole("button", { name: "到最上面" }));
      expect(scrollTo).toHaveBeenCalledWith({ behavior: "smooth", top: 0 });
  });

  it("shows sender and full timestamp on one line without an avatar", () => {
    const { container } = render(
      <MessageTimeline token="test-token" conversation={conversation} messages={messages} participants={[]} />,
    );
    expect(container.querySelector(".sender-avatar")).toBeNull();
    const firstMeta = container.querySelector(".message-meta");
    expect(firstMeta).not.toBeNull();
    expect(firstMeta?.querySelector(".sender-name")).toHaveTextContent("成员一");
    expect(firstMeta?.querySelector(".message-time")?.textContent).toMatch(/^2026-09-15.+:\d{2}:00$/);
  });

  it("returns to the top after changing pages", () => {
    const { container, rerender } = render(
      <MessageTimeline token="test-token" conversation={conversation} messages={messages} participants={[]} pageIndex={0} />,
    );
    const scroll = container.querySelector<HTMLElement>(".message-scroll")!;
    const scrollTo = vi.fn();
    Object.defineProperty(scroll, "scrollTo", { configurable: true, value: scrollTo });
    rerender(<MessageTimeline token="test-token" conversation={conversation} messages={messages} participants={[]} pageIndex={1} />);
    expect(scrollTo).toHaveBeenCalledWith({ behavior: "auto", top: 0 });
  });

  it("renders attachment-only image and file messages without a message bubble and opens files", () => {
    const attachmentMessages: MessageItem[] = [
      {
        ...messages[0],
        id: "image-message",
        type: "image",
        body: undefined,
        attachment: { name: "photo.png", meta: "12 KB · 完整", kind: "image", contentHash: "image-hash" },
      },
      {
        ...messages[1],
        id: "file-message",
        type: "file",
        body: "report.pdf",
        attachment: { name: "report.pdf", meta: "20 KB · 完整", kind: "document", contentHash: "file-hash" },
      },
    ];
    const { container } = render(
      <MessageTimeline token="test-token" conversation={conversation} messages={attachmentMessages} participants={[]} />,
    );

    expect(container.querySelectorAll(".message-attachment-only")).toHaveLength(2);
    expect(container.querySelector(".message-bubble")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "打开附件 report.pdf" }));
    expect(mediaApi.openMediaFile).toHaveBeenCalledWith("test-token", "file-hash", "report.pdf");
  });
});
