import { render, screen, waitFor } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import { ServerConfigPage } from "./ServerConfigPage";

afterEach(() => {
  sessionStorage.clear();
  vi.unstubAllGlobals();
});

it("loads server redaction defaults before persisting a draft", async () => {
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ collectionNotice: "测试告知", keyId: "test-key", dataRedaction: true })));
  render(<ServerConfigPage token="test-token" onTokenChanged={vi.fn()} />);
  expect(sessionStorage.getItem("collector-config-draft:test-token")).toBeNull();
  await waitFor(() => expect(JSON.parse(sessionStorage.getItem("collector-config-draft:test-token") || "null")?.dataRedaction).toBe(true));
  expect(screen.getByRole("checkbox", { name: "数据脱敏" })).toBeChecked();
});

it("restores an existing navigation draft without overwriting it on mount", async () => {
  sessionStorage.setItem("collector-config-draft:test-token", JSON.stringify({ dataRedaction: false }));
  vi.stubGlobal("fetch", vi.fn(async () => Response.json({ collectionNotice: "测试告知", keyId: "test-key", dataRedaction: true })));
  render(<ServerConfigPage token="test-token" onTokenChanged={vi.fn()} />);
  await waitFor(() => expect(screen.getByRole("checkbox", { name: "数据脱敏" })).not.toBeChecked());
  expect(JSON.parse(sessionStorage.getItem("collector-config-draft:test-token") || "null").dataRedaction).toBe(false);
});
