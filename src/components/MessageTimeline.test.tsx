import { fireEvent, render, screen, waitFor } from "@testing-library/react";
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
    mediaApi.getMediaObjectUrl.mockReset().mockImplementation(() => new Promise<string>(() => undefined));
    mediaApi.openMediaFile.mockReset().mockResolvedValue({ opened: true });
  });

  it("labels a system conversation independently of the archived participant count", () => {
    render(<MessageTimeline token="test-token" conversation={{ ...conversation, kind: "system" }} messages={messages} participants={[]} />);
    expect(screen.getByText("系统会话")).toBeInTheDocument();
    expect(screen.queryByText("群会话")).not.toBeInTheDocument();
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
      expect(screen.queryByPlaceholderText("搜索当前会话消息…")).not.toBeInTheDocument();

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

  it("renders WeCom emoji tokens inline while preserving adjacent text", () => {
    const emojiMessage: MessageItem = {
      ...messages[0],
      id: "emoji-message",
      body: "[便便]99345 [未知表情]",
    };
    render(<MessageTimeline token="test-token" conversation={conversation} messages={[emojiMessage]} participants={[]} />);
    expect(screen.getByRole("img", { name: "企业微信表情：便便" })).toHaveTextContent("💩");
    expect(screen.getByText("99345 [未知表情]", { exact: false })).toBeInTheDocument();
  });

  it("renders structured emoji segments from a newly collected message", () => {
    const emojiMessage: MessageItem = {
      ...messages[0],
      id: "structured-emoji-message",
      body: "[便便]99345",
      richSegments: [{ kind: "emoji", text: "[便便]" }, { kind: "text", text: "99345" }],
    };
    render(<MessageTimeline token="test-token" conversation={conversation} messages={[emojiMessage]} participants={[]} />);
    expect(screen.getByRole("img", { name: "企业微信表情：便便" })).toHaveTextContent("💩");
    expect(screen.getByText("99345")).toBeInTheDocument();
  });

  it("renders WeCom markdown attendance cards without executing HTML", () => {
    const cardMessage: MessageItem = {
      ...messages[0],
      id: "attendance-card",
      body: "### <font size=\"4\" color=\"#333\">📋 打卡结果</font>\n\n> 打卡时间：<font color=\"info\">14:11</font>\n> 打卡设备：<font color=\"warning\">办公室</font>\n> <font color=\"comment\">2026年10月16日</font>",
    };
    const { container } = render(<MessageTimeline token="test-token" conversation={conversation} messages={[cardMessage]} participants={[]} />);
    expect(container.querySelector(".wecom-rich-card h3")).toHaveTextContent("📋 打卡结果");
    expect(container.querySelector(".wecom-card-info")).toHaveTextContent("14:11");
    expect(container.querySelector(".wecom-card-warning")).toHaveTextContent("办公室");
    expect(container.querySelector(".wecom-card-comment")).toHaveTextContent("2026年10月16日");
    expect(container.querySelector("script")).toBeNull();
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

  function showFile(name: string, contentHash?: string) {
    return render(<MessageTimeline token="test-token" conversation={conversation} messages={[{ ...messages[0], type: "file", body: name, attachment: { name, contentHash, kind: "document", meta: "附件" } }]} participants={[]} />);
  }

  it("explains how to recover a file whose content was not collected", () => {
    showFile("report.pdf");
    expect(screen.getByRole("button", { name: "打开附件 report.pdf" })).toBeDisabled();
    expect(screen.getByRole("status")).toHaveTextContent("包含全内容");
    expect(screen.queryByRole("button", { name: "保存附件 report.pdf" })).not.toBeInTheDocument();
  });

  it("requires confirmation before running an executable and supports cancellation", async () => {
    showFile("test-setup.exe", "file-hash");
    fireEvent.click(screen.getByRole("button", { name: "打开附件 test-setup.exe" }));
    expect(screen.getByRole("dialog", { name: "确认运行附件" })).toBeInTheDocument();
    expect(mediaApi.openMediaFile).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "打开附件 test-setup.exe" }));
    fireEvent.click(screen.getByRole("button", { name: "确认运行" }));
    await waitFor(() => expect(mediaApi.openMediaFile).toHaveBeenCalledWith("test-token", "file-hash", "test-setup.exe"));
  });

  it("shows the backend open error so the user can save the file instead", async () => {
    mediaApi.openMediaFile.mockRejectedValueOnce(new Error("Windows 无法打开此附件。请先保存附件。"));
    showFile("report.pdf", "file-hash");
    fireEvent.click(screen.getByRole("button", { name: "打开附件 report.pdf" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("请先保存附件");
    expect(screen.getByRole("button", { name: "保存附件 report.pdf" })).toBeEnabled();
  });

  it("saves an executable without running it and releases its object URL on unmount", async () => {
    mediaApi.getMediaObjectUrl.mockResolvedValueOnce("blob:attachment");
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
      expect(this.download).toBe("test-setup.exe");
      expect(this.href).toBe("blob:attachment");
    });
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => undefined);
    const view = showFile("test-setup.exe", "file-hash");
    fireEvent.click(screen.getByRole("button", { name: "保存附件 test-setup.exe" }));
    await waitFor(() => expect(click).toHaveBeenCalledOnce());
    expect(mediaApi.openMediaFile).not.toHaveBeenCalled();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    view.unmount();
    expect(revoke).toHaveBeenCalledWith("blob:attachment");
    click.mockRestore(); revoke.mockRestore();
  });
});
