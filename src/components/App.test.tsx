import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, vi } from "vitest";
import App from "../App";

const accessToken = "0123456789abcdefghijklmnop";

function installApiMock() {
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/archive/summary")) {
      return Response.json({ conversation_count: 1, message_count: 1, media_count: 0 });
    }
    if (url.includes("/conversations")) {
      return Response.json([{
        conversation_id: "conversation-1",
        display_name: "真实 API 会话",
        conversation_type: "direct",
        last_message_at: "2026-09-07T10:24:00Z",
        message_count: 1,
        media_count: 0,
        participant_count: 1,
      }]);
    }
    if (url.includes("/messages")) {
      return Response.json([{
        stable_message_id: "message-1",
        conversation_id: "conversation-1",
        sender_id: "participant-1",
        sent_at: "2026-09-07T10:24:00Z",
        direction: "incoming",
        message_type: "text",
        body_text: "来自服务端归档的数据",
        lifecycle: "active",
        media: [],
        raw_type: "text",
      }]);
    }
    return Response.json({}, { status: 404 });
  }));
}

async function connect() {
  fireEvent.change(screen.getByLabelText("服务端访问令牌"), { target: { value: accessToken } });
  fireEvent.click(screen.getByRole("button", { name: "验证并进入工作台" }));
  await screen.findByRole("heading", { name: "真实 API 会话", level: 1 });
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("archive workspace", () => {
  it("loads conversations and messages from authenticated APIs without persisting the token", async () => {
    installApiMock();
    const storageSpy = vi.spyOn(Storage.prototype, "setItem");
    render(<App />);
    await connect();
    expect(await screen.findByText("来自服务端归档的数据")).toBeInTheDocument();
    expect(screen.getAllByText(/已归档 1 条消息/).length).toBeGreaterThan(0);
    expect(storageSpy).not.toHaveBeenCalled();
  });

  it("requires a second confirmation for entire archive export", async () => {
    installApiMock();
    render(<App />);
    await connect();
    fireEvent.click(screen.getByText("全部档案"));
    fireEvent.click(screen.getByText("开始导出"));
    expect(screen.getByRole("heading", { name: "确认导出全部档案" })).toBeInTheDocument();
    expect(screen.getByText("ZIP 包")).toBeInTheDocument();
  });
});
