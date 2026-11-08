import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, vi } from "vitest";
import { MaintenanceApp } from "./MaintenanceApp";
import { activateRestored, createBackup, maintenanceStatus, restoreBackup, verifyBackup } from "../lib/maintenance";

vi.mock("../lib/maintenance", () => ({
  activateRestored: vi.fn(), createBackup: vi.fn(), maintenanceStatus: vi.fn(), restoreBackup: vi.fn(), verifyBackup: vi.fn(),
}));

beforeEach(() => { vi.clearAllMocks(); vi.mocked(maintenanceStatus).mockResolvedValue(false); });
afterEach(() => { delete window.__WECOM_BACKUP_LOCATION__; });

it("opens backups for review without exposing creation or activation", async () => {
  window.__WECOM_BACKUP_LOCATION__ = true;
  vi.mocked(maintenanceStatus).mockResolvedValue(true);
  render(<MaintenanceApp />);
  expect(screen.getByRole("note")).toHaveTextContent("当前程序位于备份目录");
  await waitFor(() => expect(screen.getByRole("button", { name: "选择备份并恢复" })).toBeEnabled());
  expect(screen.queryByRole("button", { name: "选择位置并创建备份" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "确认恢复并打开工作台" })).not.toBeInTheDocument();
  expect(activateRestored).not.toHaveBeenCalled();
});

it("performs no backup or restore until selected, and prevents concurrent actions", async () => {
  let finish!: (value: null) => void;
  vi.mocked(createBackup).mockReturnValue(new Promise((resolve) => { finish = resolve; }));
  render(<MaintenanceApp />);
  const create = screen.getByRole("button", { name: "选择位置并创建备份" });
  await waitFor(() => expect(create).toBeEnabled());
  expect(createBackup).not.toHaveBeenCalled();
  expect(restoreBackup).not.toHaveBeenCalled();
  fireEvent.click(create);
  expect(createBackup).toHaveBeenCalledWith();
  expect(screen.getByRole("button", { name: "选择备份并恢复" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "选择备份并校验" })).toBeDisabled();
  await act(async () => { finish(null); });
  expect(screen.getByRole("status")).toHaveTextContent("已取消目录选择");
  expect(create).toBeEnabled();
});

it("displays a verified result and clears it after a subsequent failure", async () => {
  vi.mocked(verifyBackup).mockResolvedValue({ directory: "synthetic-backup", fileCount: 5, bytes: 1024 });
  vi.mocked(restoreBackup).mockRejectedValue("备份校验失败，原数据未修改。");
  render(<MaintenanceApp />);
  const verify = screen.getByRole("button", { name: "选择备份并校验" });
  await waitFor(() => expect(verify).toBeEnabled());
  fireEvent.click(verify);
  await screen.findByText("目录：synthetic-backup");
  expect(screen.getByRole("status")).toHaveTextContent("校验备份完成");
  fireEvent.click(screen.getByRole("button", { name: "选择备份并恢复" }));
  await screen.findByRole("alert");
  expect(screen.queryByText("目录：synthetic-backup")).not.toBeInTheDocument();
  expect(screen.queryByText("校验备份完成。")).not.toBeInTheDocument();
});

it("requires an explicit confirmation before activating restored schedules and identity", async () => {
  vi.mocked(maintenanceStatus).mockResolvedValue(true);
  vi.mocked(activateRestored).mockRejectedValue("恢复确认未完成，请重试。");
  render(<MaintenanceApp />);
  const activate = await screen.findByRole("button", { name: "确认恢复并打开工作台" });
  expect(activateRestored).not.toHaveBeenCalled();
  expect(screen.getByText(/本机采集计划会暂停/)).toBeInTheDocument();
  fireEvent.click(activate);
  expect(activateRestored).toHaveBeenCalledWith();
  await screen.findByRole("alert");
  expect(activate).toBeEnabled();
});
