import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { ServerConfigPage } from "./ServerConfigPage";

afterEach(() => {
  sessionStorage.clear();
  vi.unstubAllGlobals();
});

it("loads server redaction defaults before persisting a draft", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ keyId: "test-key", dataRedaction: true })));
  render(<ServerConfigPage token="test-token" onTokenChanged={vi.fn()} />);
  expect(sessionStorage.getItem("collector-config-draft:test-token")).toBeNull();
  await waitFor(() => expect(JSON.parse(sessionStorage.getItem("collector-config-draft:test-token") || "null")?.dataRedaction).toBe(true));
  fireEvent.click(screen.getByRole("button", { name: "生成默认配置" }));
  expect(screen.getByRole("checkbox", { name: "数据脱敏" })).toBeChecked();
  const collectorLogging = screen.getByRole("checkbox", { name: "开启采集端日志" });
  expect(collectorLogging).not.toBeChecked();
  expect(collectorLogging.closest(".collector-media-options")).not.toBeNull();
  fireEvent.click(collectorLogging);
  expect(collectorLogging).toBeChecked();
  fireEvent.click(screen.getByRole("radio", { name: "按间隔" }));
  expect(screen.getByLabelText("间隔（分钟）").parentElement).toHaveTextContent("分钟");
});

it("restores an existing navigation draft without overwriting it on mount", async () => {
  sessionStorage.setItem("collector-config-draft:test-token", JSON.stringify({ dataRedaction: false }));
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ keyId: "test-key", dataRedaction: true })));
  render(<ServerConfigPage token="test-token" onTokenChanged={vi.fn()} />);
  fireEvent.click(screen.getByRole("button", { name: "生成默认配置" }));
  await waitFor(() => expect(screen.getByRole("checkbox", { name: "数据脱敏" })).not.toBeChecked());
  expect(JSON.parse(sessionStorage.getItem("collector-config-draft:test-token") || "null").dataRedaction).toBe(false);
});
