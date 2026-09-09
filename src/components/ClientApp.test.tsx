import { fireEvent, render, screen } from "@testing-library/react";
import { vi } from "vitest";
import ClientApp from "../ClientApp";
import { backend } from "../lib/backend";

describe("portable client", () => {
  it("automatically prepares and exports without a setup wizard", async () => {
    render(<ClientApp />);
    expect(screen.getByRole("heading", { name: "正在准备" })).toBeInTheDocument();
    expect(await screen.findByRole("heading", { name: "可以导出" })).toBeInTheDocument();
    expect(screen.queryByText("手动输入密钥")).not.toBeInTheDocument();
    expect(screen.queryByText("选择目录")).not.toBeInTheDocument();
    expect(screen.queryByText("企业微信记录归档")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("导出格式")).not.toBeInTheDocument();
    expect(screen.queryByText("JSON")).not.toBeInTheDocument();
    expect(screen.queryByText("CSV")).not.toBeInTheDocument();

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
});
