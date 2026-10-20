import type { ConversationSummary } from "./types";

const kindLabels: Record<NonNullable<ConversationSummary["kind"]>, string> = {
  document: "文档会话",
  application: "应用会话",
  assistant: "应用会话",
  temporary: "临时会话",
  system: "系统会话",
};

export function conversationKindLabel(conversation: ConversationSummary): string {
  return conversation.kind ? kindLabels[conversation.kind] : conversation.isGroup ? "群会话" : "私人会话";
}
