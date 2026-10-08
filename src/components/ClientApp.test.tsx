import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import ClientApp from "../ClientApp";
import { backend } from "../lib/backend";
import * as events from "@tauri-apps/api/event";
import type { CollectionSchedule } from "../domain/types";
import type { CollectorScheduleStatus } from "../lib/backend";

vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

afterEach(() => vi.restoreAllMocks());

describe("collector", () => {
  it("clears the first upload error after a later background upload succeeds", async () => {
    let currentStatus: CollectorScheduleStatus = { running: false, lastError: "合成上传失败" };
    let poll: (() => Promise<void>) | undefined;
    vi.spyOn(window, "setInterval").mockImplementation((callback, delay) => {
      if (delay === 5000) poll = callback as () => Promise<void>;
      return 1;
    });
    vi.spyOn(backend, "bootstrap").mockResolvedValue({ organizationName: "测试组织", collectorSchedule: { mode: "interval", intervalMinutes: 60, dailyTime: "02:00" } });
    vi.spyOn(backend, "uploadLatest").mockRejectedValue(new Error("合成上传失败"));
    vi.spyOn(backend, "getCollectorScheduleStatus").mockImplementation(async () => currentStatus);
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "测试组织采集端" });
    expect(screen.getByRole("status")).toHaveTextContent("合成上传失败");
    expect(screen.getByText("执行失败")).toBeInTheDocument();

    currentStatus = { running: false, lastSuccessAt: "2026-09-24T02:00:00Z", uploadedCount: 238 };
    await act(async () => { await poll?.(); });
    expect(screen.queryByText("合成上传失败")).not.toBeInTheDocument();
    expect(screen.getByText("运行正常")).toBeInTheDocument();
  });

  it("updates the displayed plan when a disabled collector receives an enabled schedule", async () => {
    let onSchedule: events.EventCallback<CollectionSchedule> | undefined;
    const dispose = vi.fn();
    vi.mocked(events.listen).mockImplementation(async (event, callback) => {
      if (event === "collector:schedule-updated") onSchedule = callback as events.EventCallback<CollectionSchedule>;
      return dispose;
    });
    vi.spyOn(backend, "isNative").mockReturnValue(true);
    vi.spyOn(backend, "bootstrap").mockResolvedValue({ displayName: "测试采集端", offlineExportEnabled: false, collectorSchedule: { mode: "disabled", intervalMinutes: 60, dailyTime: "02:00" } });
    vi.spyOn(backend, "discoverSources").mockResolvedValue([{ sourceId: "fixture-source", displayPath: "fixture", capability: "probe_required", databases: [] }]);
    vi.spyOn(backend, "collectSourceAutomatically").mockResolvedValue({ exportId: "fixture-export", generatedAt: "2026-09-24T01:00:00Z", messageCount: 238, mediaCount: 0, missingMediaCount: 0, contentSha256Prefix: "fixture-hash" });
    vi.spyOn(backend, "getCollectorScheduleStatus").mockResolvedValue({ running: false });
    const view = render(<ClientApp />);
    await screen.findByRole("heading", { name: "测试采集端" });
    expect(screen.getByText("采集计划：").parentElement).toHaveTextContent("未启用");
    expect(onSchedule).toBeDefined();
    act(() => onSchedule?.({ event: "collector:schedule-updated", id: 1, payload: { mode: "interval", intervalMinutes: 30, dailyTime: "02:00" } }));
    expect(screen.getByText("每 30 分钟自动采集并上传")).toBeInTheDocument();
    view.unmount();
    expect(dispose).toHaveBeenCalledTimes(2);
  });

  it("performs the first automatic upload immediately when a continuous plan is enabled", async () => {
    vi.spyOn(backend, "bootstrap").mockResolvedValue({ organizationName: "计划企业", offlineExportEnabled: false, collectorSchedule: { mode: "interval", intervalMinutes: 15, dailyTime: "02:00" } });
    const uploadLatest = vi.spyOn(backend, "uploadLatest").mockResolvedValue({ messageCount: 238 });
    const hide = vi.spyOn(backend, "hideCollectorWindow").mockResolvedValue();
    render(<ClientApp />);

    expect(await screen.findByRole("heading", { name: "计划企业采集端" })).toBeInTheDocument();
    expect(screen.getByText("每 15 分钟自动采集并上传")).toBeInTheDocument();
    expect(screen.queryByText(/完成首次自动采集并上传/)).not.toBeInTheDocument();
    expect(screen.queryByText(/关闭窗口后仍在托盘运行/)).not.toBeInTheDocument();
    expect(uploadLatest).toHaveBeenCalledOnce();
    await waitFor(() => expect(hide).not.toHaveBeenCalled());
  });

  it("renders successful upload metrics on separate rows and refreshes the running status", async () => {
    const succeededAt = "2026-09-24T01:38:30Z";
    let currentStatus = { running: false, lastSuccessAt: succeededAt, nextRunAt: "2026-09-24 10:38:30", uploadedCount: 123456, lastMessageCount: 123456, lastMediaCount: 0 };
    let poll: (() => Promise<void>) | undefined;
    vi.spyOn(window, "setInterval").mockImplementation((callback, delay) => {
      if (delay === 5000) poll = callback as () => Promise<void>;
      return 1;
    });
    vi.spyOn(backend, "bootstrap").mockResolvedValue({ organizationName: "测试组织", offlineExportEnabled: false, collectorSchedule: { mode: "interval", intervalMinutes: 60, dailyTime: "02:00" } });
    vi.spyOn(backend, "collectSourceAutomatically").mockResolvedValue({ exportId: "test-export", generatedAt: succeededAt, messageCount: 123456, mediaCount: 0, missingMediaCount: 0, contentSha256Prefix: "test-hash" });
    vi.spyOn(backend, "getCollectorScheduleStatus").mockImplementation(async () => currentStatus);
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "测试组织采集端" });
    const successRow = screen.getByText("最近成功：").parentElement!;
    expect(within(successRow).getByText(new Date(succeededAt).toLocaleString("zh-CN", { hour12: false }))).toBeInTheDocument();
    expect(successRow.tagName).toBe("DIV");
    expect(successRow.parentElement?.tagName).toBe("DL");
    expect(screen.getByText("下次执行：").parentElement).toHaveTextContent(new Date("2026-09-24T10:38:30").toLocaleString("zh-CN", { hour12: false }));
    expect(screen.getByText("已上传：").parentElement).toHaveTextContent("123,456 条");
    expect(screen.queryByText("媒体引用：")).not.toBeInTheDocument();
    expect(screen.getByText("运行正常")).toBeInTheDocument();

    currentStatus = { ...currentStatus, running: true };
    await act(async () => { await poll?.(); });
    expect(screen.getByText("正在采集")).toBeInTheDocument();
  });

  it("automatically prepares local data and uploads it to the archive workspace", async () => {
    const uploadLatest = vi.spyOn(backend, "uploadLatest");
    render(<ClientApp />);

    expect(screen.getByRole("heading", { name: "正在采集本机数据" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "示例采集端" })).toBeInTheDocument();
    expect(screen.queryByText("导出格式")).not.toBeInTheDocument();
    expect(screen.queryByText("选择目录")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "导出加密文件" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "上传到归档工作台" }));
    expect(await screen.findByRole("heading", { name: "上传完成" })).toBeInTheDocument();
    expect(screen.getByText(/共 238 条消息/)).toBeInTheDocument();
    expect(uploadLatest).toHaveBeenCalledOnce();
  });

  it("keeps the ready page visible when a status refresh has no collection count", async () => {
    vi.spyOn(backend, "getCollectorScheduleStatus").mockResolvedValue({ running: false, lastMessageCount: null as unknown as number, lastMediaCount: null as unknown as number });
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "示例采集端" });
    await waitFor(() => expect(backend.getCollectorScheduleStatus).toHaveBeenCalled());
    expect(screen.getByText("238")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "上传到归档工作台" })).toBeEnabled();
  });

  it("exports an encrypted offline package and offers its directory", async () => {
    const exportLatest = vi.spyOn(backend, "exportLatestEncrypted");
    const openDirectory = vi.spyOn(backend, "openOfflineExportDirectory");
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "示例采集端" });

    fireEvent.click(screen.getByRole("button", { name: "导出加密文件" }));
    expect(await screen.findByRole("heading", { name: "导出完成" })).toBeInTheDocument();
    expect(screen.getByText(/wecom-20260924-182016\.wca/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "打开目录" }));
    expect(exportLatest).toHaveBeenCalledOnce();
    expect(openDirectory).toHaveBeenCalledOnce();
  });

  it("locks upload while it is running and reports a recoverable failure", async () => {
    let rejectUpload!: (reason: Error) => void;
    vi.spyOn(backend, "uploadLatest").mockImplementation(() => new Promise((_, reject) => { rejectUpload = reject; }));
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "示例采集端" });

    fireEvent.click(screen.getByRole("button", { name: "上传到归档工作台" }));
    await waitFor(() => expect(backend.uploadLatest).toHaveBeenCalledOnce());
    expect(screen.getByRole("button", { name: "正在上传…" })).toBeDisabled();
    rejectUpload(new Error("合成上传失败"));

    expect(await screen.findByRole("heading", { name: "上传失败" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "确认" }));
    expect(screen.getByRole("button", { name: "上传到归档工作台" })).toBeEnabled();
  });
});
