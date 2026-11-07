import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, vi } from "vitest";
import { MessageTimeline } from "./MessageTimeline";
import type { ConversationSummary, MessageItem } from "../domain/types";

const media = vi.hoisted(() => ({ getMediaObjectUrl: vi.fn(), openMediaFile: vi.fn() }));
vi.mock("../lib/server-api", () => media);

const conversation: ConversationSummary = {
  id: "synthetic-conversation", title: "合成会话", initials: "合", accent: "blue", lastMessage: "图片",
  lastAt: "", unread: 0, messageCount: 100, mediaCount: 100, participantCount: 1, isGroup: true,
};
const image: MessageItem = {
  id: "synthetic-image", conversationId: conversation.id, senderId: "synthetic-sender", senderName: "成员",
  senderInitial: "成", sentAt: "2026-11-01T00:00:00Z", timeLabel: "08:00", direction: "incoming",
  type: "image", body: "", lifecycle: "active",
  attachment: { kind: "image", name: "synthetic.png", contentHash: "synthetic-hash", meta: "图片" },
};
const timeline = (rows = [image], token = "synthetic-token") => <MessageTimeline token={token} conversation={conversation} messages={rows} participants={[]} />;

class VisibilityObserver {
  static instances: VisibilityObserver[] = [];
  observe = vi.fn();
  disconnect = vi.fn();
  constructor(readonly callback: IntersectionObserverCallback, readonly options: IntersectionObserverInit) {
    VisibilityObserver.instances.push(this);
  }
  notify(visible = true) {
    this.callback([{ target: this.observe.mock.calls[0][0], isIntersecting: visible } as IntersectionObserverEntry], this as unknown as IntersectionObserver);
  }
}

beforeEach(() => {
  VisibilityObserver.instances = [];
  vi.stubGlobal("IntersectionObserver", VisibilityObserver);
  media.getMediaObjectUrl.mockReset().mockImplementation(() => new Promise<string>(() => undefined));
  vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => undefined);
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("does not fetch a 100-image page until individual images approach the scroll viewport", () => {
  const rows = Array.from({ length: 100 }, (_, index) => ({ ...image, id: `image-${index}`, attachment: { ...image.attachment!, contentHash: `hash-${index}` } }));
  const view = render(timeline(rows));
  expect(media.getMediaObjectUrl).not.toHaveBeenCalled();
  const [first, second] = VisibilityObserver.instances;
  expect(first.options).toEqual({ root: view.container.querySelector(".message-scroll"), rootMargin: "120px 0px" });
  act(() => first.notify(false));
  expect(media.getMediaObjectUrl).not.toHaveBeenCalled();
  act(() => { first.notify(); second.notify(); first.notify(); });
  expect(media.getMediaObjectUrl).toHaveBeenCalledTimes(2);
  expect(media.getMediaObjectUrl.mock.calls.map((args) => args[1])).toEqual(["hash-0", "hash-1"]);
  expect(first.disconnect).toHaveBeenCalled();
});

it("shares an in-flight visibility request with clicks and reuses its URL for preview", async () => {
  let resolve!: (url: string) => void;
  media.getMediaObjectUrl.mockImplementationOnce(() => new Promise<string>((done) => { resolve = done; }));
  const view = render(timeline());
  act(() => VisibilityObserver.instances[0].notify());
  fireEvent.click(screen.getByRole("button", { name: "查看图片 synthetic.png" }));
  expect(media.getMediaObjectUrl).toHaveBeenCalledOnce();
  const signal = media.getMediaObjectUrl.mock.calls[0][2] as AbortSignal;
  await act(async () => resolve("blob:visible-image"));
  expect(await screen.findByRole("dialog", { name: "图片预览 synthetic.png" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "关闭图片预览" }));
  fireEvent.click(screen.getByRole("button", { name: "查看图片 synthetic.png" }));
  await screen.findByRole("dialog");
  expect(media.getMediaObjectUrl).toHaveBeenCalledOnce();
  view.unmount();
  expect(signal.aborted).toBe(true);
  expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:visible-image");
});

it("starts on click before visibility notification and permits retry after a read failure", async () => {
  media.getMediaObjectUrl.mockRejectedValueOnce(new Error("synthetic unavailable"));
  media.getMediaObjectUrl.mockResolvedValueOnce("blob:retried-image");
  render(timeline());
  fireEvent.click(screen.getByRole("button", { name: "查看图片 synthetic.png" }));
  await screen.findByText("图片读取失败，请点击重试。");
  expect(media.getMediaObjectUrl).toHaveBeenCalledOnce();
  fireEvent.click(screen.getByRole("button", { name: "查看图片 synthetic.png" }));
  await screen.findByRole("dialog");
  expect(media.getMediaObjectUrl).toHaveBeenCalledTimes(2);
});

it("aborts replaced-page requests and revokes late URLs without updating the new page", async () => {
  let resolve!: (url: string) => void;
  media.getMediaObjectUrl.mockImplementationOnce(() => new Promise<string>((done) => { resolve = done; }));
  const view = render(timeline());
  act(() => VisibilityObserver.instances[0].notify());
  fireEvent.click(screen.getByRole("button", { name: "查看图片 synthetic.png" }));
  const signal = media.getMediaObjectUrl.mock.calls[0][2] as AbortSignal;
  view.rerender(timeline([]));
  expect(signal.aborted).toBe(true);
  expect(VisibilityObserver.instances[0].disconnect).toHaveBeenCalled();
  await act(async () => resolve("blob:late-image"));
  expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:late-image");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it.each(["token", "content"])("discards the prior image when its %s changes on the same message", async (change) => {
  media.getMediaObjectUrl.mockResolvedValueOnce("blob:old-image");
  const view = render(timeline());
  act(() => VisibilityObserver.instances[0].notify());
  await waitFor(() => expect(screen.getByRole("img", { name: "synthetic.png" })).toHaveAttribute("src", "blob:old-image"));
  const updated = change === "content" ? { ...image, attachment: { ...image.attachment!, contentHash: "updated-hash" } } : image;
  view.rerender(timeline([updated], change === "token" ? "updated-token" : "synthetic-token"));
  expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:old-image");
  expect(screen.queryByRole("img", { name: "synthetic.png" })).not.toBeInTheDocument();
  expect(media.getMediaObjectUrl).toHaveBeenCalledOnce();
  act(() => VisibilityObserver.instances[1].notify());
  expect(media.getMediaObjectUrl.mock.calls[1].slice(0, 2)).toEqual([change === "token" ? "updated-token" : "synthetic-token", change === "content" ? "updated-hash" : "synthetic-hash"]);
});

it("leaves uncollected images disabled without scheduling a request", () => {
  render(timeline([{ ...image, attachment: { ...image.attachment!, contentHash: undefined } }]));
  expect(screen.getByRole("button", { name: "查看图片 synthetic.png" })).toBeDisabled();
  expect(screen.getByText("未采集图片内容")).toBeInTheDocument();
  expect(VisibilityObserver.instances).toHaveLength(0);
  expect(media.getMediaObjectUrl).not.toHaveBeenCalled();
});

it("falls back to explicit clicks without eager fetching when visibility observation is unavailable", async () => {
  vi.stubGlobal("IntersectionObserver", undefined);
  media.getMediaObjectUrl.mockResolvedValueOnce("blob:clicked-image");
  render(timeline());
  expect(media.getMediaObjectUrl).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole("button", { name: "查看图片 synthetic.png" }));
  await screen.findByRole("dialog");
  expect(media.getMediaObjectUrl).toHaveBeenCalledOnce();
});

it("loads normally after StrictMode replays setup and cleans up its observer", async () => {
  media.getMediaObjectUrl.mockResolvedValueOnce("blob:strict-image");
  const view = render(<StrictMode>{timeline()}</StrictMode>);
  expect(VisibilityObserver.instances[0].disconnect).toHaveBeenCalled();
  act(() => VisibilityObserver.instances.at(-1)!.notify());
  await screen.findByRole("img", { name: "synthetic.png" });
  view.unmount();
  expect(URL.revokeObjectURL).toHaveBeenCalledExactlyOnceWith("blob:strict-image");
});
