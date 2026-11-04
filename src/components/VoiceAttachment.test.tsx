import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, vi } from "vitest";
import { VoiceAttachment } from "./VoiceAttachment";

const mocks = vi.hoisted(() => ({ getMediaBlob: vi.fn(), voicePlaybackBlob: vi.fn() }));
vi.mock("../lib/server-api", () => ({ getMediaBlob: mocks.getMediaBlob }));
vi.mock("../lib/voice-playback", async (importOriginal) => ({ ...await importOriginal<typeof import("../lib/voice-playback")>(), voicePlaybackBlob: mocks.voicePlaybackBlob }));
const attachment = { name: "voice.silk", meta: "6 KB · 完整", kind: "audio" as const, contentHash: "synthetic-voice-hash" };
const bytes = new Uint8Array([...new TextEncoder().encode("#!SILK_V3"), ...Array.from({ length: 150 }, () => [1, 0, 42]).flat()]);
const originalBlob = { size: bytes.length, arrayBuffer: async () => bytes.buffer } as Blob;

beforeEach(() => {
  mocks.getMediaBlob.mockReset().mockResolvedValue(originalBlob);
  mocks.voicePlaybackBlob.mockReset().mockResolvedValue(new Blob(["decoded WAV"], { type: "audio/wav" }));
  vi.spyOn(HTMLMediaElement.prototype, "play").mockImplementation(async function (this: HTMLMediaElement) {
    Object.defineProperty(this, "paused", { configurable: true, value: false });
    this.dispatchEvent(new Event("play"));
  });
  vi.spyOn(HTMLMediaElement.prototype, "pause").mockImplementation(function (this: HTMLMediaElement) {
    Object.defineProperty(this, "paused", { configurable: true, value: true });
    this.dispatchEvent(new Event("pause"));
  });
  vi.spyOn(HTMLMediaElement.prototype, "load").mockImplementation(() => undefined);
  vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:voice");
  vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => undefined);
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); });

it("plays and pauses inside the voice bubble, displays progress and releases the decoded audio", async () => {
  const view = render(<VoiceAttachment token="synthetic-token" attachment={attachment} />);
  expect(await screen.findByText("3″")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "播放语音" }));
  const pause = await screen.findByRole("button", { name: "暂停语音" });
  expect(pause).toHaveAttribute("aria-pressed", "true");
  const audio = view.container.querySelector("audio")!;
  Object.defineProperty(audio, "duration", { configurable: true, value: 3 });
  audio.currentTime = 1;
  fireEvent.timeUpdate(audio);
  expect(screen.getByLabelText("语音时长")).toHaveTextContent("1″ / 3″");
  fireEvent.click(pause);
  expect(screen.getByRole("button", { name: "播放语音" })).toHaveAttribute("aria-pressed", "false");
  fireEvent.click(screen.getByRole("button", { name: "播放语音" }));
  await screen.findByRole("button", { name: "暂停语音" });
  expect(mocks.voicePlaybackBlob).toHaveBeenCalledOnce();
  view.unmount();
  expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:voice");
});

it("saves the original SILK from the inline icon without decoding or opening an application", async () => {
  const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) {
    expect(this.download).toBe("voice.silk");
  });
  const view = render(<VoiceAttachment token="synthetic-token" attachment={attachment} />);
  fireEvent.click(screen.getByRole("button", { name: "保存语音" }));
  await waitFor(() => expect(click).toHaveBeenCalledOnce());
  expect(URL.createObjectURL).toHaveBeenCalledWith(originalBlob);
  expect(mocks.voicePlaybackBlob).not.toHaveBeenCalled();
  expect(view.container.querySelector(".voice-row")?.querySelectorAll("button")).toHaveLength(2);
  expect(screen.queryByRole("button", { name: "打开语音消息" })).not.toBeInTheDocument();
  expect(screen.queryByText("保存语音")).not.toBeInTheDocument();
});

it("keeps saving available after a decode error and disables missing voice actions", async () => {
  mocks.voicePlaybackBlob.mockRejectedValueOnce(new Error("synthetic decoder error"));
  const view = render(<VoiceAttachment token="synthetic-token" attachment={attachment} />);
  fireEvent.click(screen.getByRole("button", { name: "播放语音" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("可以保存原文件");
  expect(screen.getByRole("button", { name: "保存语音" })).toBeEnabled();
  view.rerender(<VoiceAttachment token="synthetic-token" attachment={{ ...attachment, contentHash: undefined }} />);
  expect(screen.getByRole("button", { name: "播放语音" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "保存语音" })).toBeDisabled();
  expect(screen.getByRole("status")).toHaveTextContent("包含全内容");
});
