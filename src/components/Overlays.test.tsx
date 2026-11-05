import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { vi } from "vitest";
import { ExportDialog } from "./Overlays";
import type { ConversationSummary } from "../domain/types";

it("keeps selected export conversations editable when the directory page is reset", async () => {
  const choices = [1, 2].map((id): ConversationSummary => ({
    id: `synthetic-${id}`, title: `合成会话 ${id}`, initials: "合成", accent: "blue",
    lastMessage: "合成记录", lastAt: "--", unread: 0, messageCount: 1, mediaCount: 0,
    participantCount: 0, isGroup: false,
  }));
  const onConfirm = vi.fn(async () => undefined);
  const props = {
    token: "synthetic-token", scope: "selected_conversations" as const, format: "json" as const,
    dataRedaction: true, simplify: true, pretty: true, conversationOrder: "descending" as const,
    messageOrder: "descending" as const, onConversationOrderChange: vi.fn(), onMessageOrderChange: vi.fn(),
    onSimplifyChange: vi.fn(), onPrettyChange: vi.fn(), messageCount: 2, mediaCount: 0,
    conversations: choices, selectedConversationId: "synthetic-2", onScopeChange: vi.fn(),
    onFormatChange: vi.fn(), onDataRedactionChange: vi.fn(), onOpenDirectoryFailed: vi.fn(),
    onClose: vi.fn(), onConfirm,
  };
  const { rerender } = render(<ExportDialog {...props} />);
  rerender(<ExportDialog {...props} conversations={[choices[0]]} />);
  const retained = screen.getByText("合成会话 2").closest("label")!.querySelector("input")!;
  expect(retained).toBeChecked();
  fireEvent.click(retained);
  fireEvent.click(screen.getByText("合成会话 1").closest("label")!.querySelector("input")!);
  fireEvent.click(screen.getByText("开始导出"));
  await waitFor(() => expect(onConfirm).toHaveBeenCalledWith(
    { conversationIds: ["synthetic-1"], startDate: undefined, endDate: undefined },
    expect.any(AbortSignal), expect.any(Function),
  ));
});
