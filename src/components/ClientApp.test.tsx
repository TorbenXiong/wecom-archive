import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import ClientApp from "../ClientApp";
import { backend } from "../lib/backend";

afterEach(() => vi.restoreAllMocks());

describe("collector", () => {
  it("performs the first automatic upload immediately when a continuous plan is enabled", async () => {
    vi.spyOn(backend, "bootstrap").mockResolvedValue({ organizationName: "计划企业", collectionNotice: "测试告知", offlineExportEnabled: false, collectorSchedule: { mode: "interval", intervalMinutes: 15, dailyTime: "02:00" } });
    const uploadLatest = vi.spyOn(backend, "uploadLatest").mockResolvedValue({ messageCount: 238 });
    const hide = vi.spyOn(backend, "hideCollectorWindow").mockResolvedValue();
    render(<ClientApp />);

    expect(await screen.findByText(/完成首次自动采集并上传，之后将按计划自动采集并上传/)).toBeInTheDocument();
    expect(uploadLatest).toHaveBeenCalledOnce();
    await waitFor(() => expect(hide).toHaveBeenCalledOnce());
  });

  it("automatically prepares local data and uploads it to the archive workspace", async () => {
    const uploadLatest = vi.spyOn(backend, "uploadLatest");
    render(<ClientApp />);

    expect(screen.getByRole("heading", { name: "正在准备" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "示例组织采集端" })).toBeInTheDocument();
    expect(screen.queryByText("导出格式")).not.toBeInTheDocument();
    expect(screen.queryByText("选择目录")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "导出加密文件" })).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "上传到归档工作台" }));
    expect(await screen.findByRole("heading", { name: "上传完成" })).toBeInTheDocument();
    expect(screen.getByText(/共 238 条消息/)).toBeInTheDocument();
    expect(uploadLatest).toHaveBeenCalledOnce();
  });

  it("exports an encrypted offline package and offers its directory", async () => {
    const exportLatest = vi.spyOn(backend, "exportLatestEncrypted");
    const openDirectory = vi.spyOn(backend, "openOfflineExportDirectory");
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "示例组织采集端" });

    fireEvent.click(screen.getByRole("button", { name: "导出加密文件" }));
    expect(await screen.findByRole("heading", { name: "导出完成" })).toBeInTheDocument();
    expect(screen.getByText(/WeComArchive-preview\.wca/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "打开目录" }));
    expect(exportLatest).toHaveBeenCalledOnce();
    expect(openDirectory).toHaveBeenCalledOnce();
  });

  it("locks upload while it is running and reports a recoverable failure", async () => {
    let rejectUpload!: (reason: Error) => void;
    vi.spyOn(backend, "uploadLatest").mockImplementation(() => new Promise((_, reject) => { rejectUpload = reject; }));
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "示例组织采集端" });

    fireEvent.click(screen.getByRole("button", { name: "上传到归档工作台" }));
    await waitFor(() => expect(backend.uploadLatest).toHaveBeenCalledOnce());
    expect(screen.getByRole("button", { name: "正在上传…" })).toBeDisabled();
    rejectUpload(new Error("合成上传失败"));

    expect(await screen.findByRole("heading", { name: "上传失败" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "确认" }));
    expect(screen.getByRole("button", { name: "上传到归档工作台" })).toBeEnabled();
  });
});
