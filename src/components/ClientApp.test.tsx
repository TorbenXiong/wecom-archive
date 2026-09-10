import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import ClientApp from "../ClientApp";
import { backend } from "../lib/backend";

afterEach(() => vi.restoreAllMocks());

describe("portable client", () => {
  it("automatically prepares and exports without a setup wizard", async () => {
    render(<ClientApp />);
    expect(screen.getByRole("heading", { name: "正在准备" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "可以导出" })).toBeInTheDocument();
    expect(screen.queryByText("准备完成，请仅导出您有权归档的数据。")).not.toBeInTheDocument();
    expect(screen.queryByText("手动输入密钥")).not.toBeInTheDocument();
    expect(screen.queryByText("选择目录")).not.toBeInTheDocument();
    expect(screen.queryByText("企业微信记录归档")).not.toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "JSON" })).toBeChecked();
    expect(screen.queryAllByRole("option")).toHaveLength(0);

    const exportLatest = vi.spyOn(backend, "exportLatest");
    fireEvent.click(screen.getByRole("button", { name: "导出" }));
    expect(await screen.findByText(/client-export-preview\.json/)).toBeInTheDocument();
    expect(exportLatest).toHaveBeenCalledWith("json", "preview-export");
    exportLatest.mockRestore();
    const openFolder = vi.spyOn(backend, "openExportFolder").mockResolvedValue();
    fireEvent.click(screen.getByRole("button", { name: "打开文件夹" }));
    expect(openFolder).toHaveBeenCalledOnce();
    openFolder.mockRestore();
    fireEvent.click(screen.getByRole("button", { name: "确认" }));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it.each(["csv", "html", "txt"] as const)("exports the selected %s format", async (format) => {
    const exportLatest = vi.spyOn(backend, "exportLatest");
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "可以导出" });
    fireEvent.click(screen.getByRole("radio", { name: format.toUpperCase() }));
    fireEvent.click(screen.getByRole("button", { name: "导出" }));
    expect(await screen.findByText(new RegExp(`client-export-preview\\.${format}`))).toBeInTheDocument();
    expect(exportLatest).toHaveBeenCalledWith(format, "preview-export");
  });

  it("keeps the format and allows retry after cancelling directory selection", async () => {
    vi.spyOn(backend, "pickDirectory").mockResolvedValue(undefined);
    const exportLatest = vi.spyOn(backend, "exportLatest");
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "可以导出" });
    fireEvent.click(screen.getByRole("radio", { name: "CSV" }));
    fireEvent.click(screen.getByRole("button", { name: "导出" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "导出" })).toBeEnabled());
    expect(exportLatest).not.toHaveBeenCalled();
    expect(screen.getByRole("radio", { name: "CSV" })).toBeChecked();
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("locks format selection during export and recovers after a failure", async () => {
    let rejectExport!: (reason: Error) => void;
    vi.spyOn(backend, "exportLatest").mockImplementation(() => new Promise((_, reject) => { rejectExport = reject; }));
    render(<ClientApp />);
    await screen.findByRole("heading", { name: "可以导出" });
    fireEvent.click(screen.getByRole("button", { name: "导出" }));
    await waitFor(() => expect(backend.exportLatest).toHaveBeenCalledOnce());
    expect(screen.getByRole("radio", { name: "JSON" })).toBeDisabled();
    rejectExport(new Error("合成导出失败"));
    expect(await screen.findByRole("heading", { name: "导出失败" })).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "JSON" })).toBeEnabled();
    expect(screen.queryByRole("button", { name: "打开文件夹" })).not.toBeInTheDocument();
  });
});
