export type MessageType =
  | "text"
  | "image"
  | "audio"
  | "video"
  | "file"
  | "link"
  | "reply"
  | "system"
  | "unsupported";

export type MessageDirection = "incoming" | "outgoing" | "system" | "unknown";

export interface ConversationSummary {
  id: string;
  title: string;
  initials: string;
  accent: "blue" | "slate" | "violet" | "amber" | "teal" | "orange";
  lastMessage: string;
  lastAt: string;
  unread: number;
  messageCount: number;
  mediaCount: number;
  participantCount: number;
}

export interface MessageItem {
  id: string;
  conversationId: string;
  senderId: string;
  senderName: string;
  senderInitial: string;
  sentAt: string;
  timeLabel: string;
  direction: MessageDirection;
  type: MessageType;
  body?: string;
  quote?: {
    sender: string;
    time: string;
    body: string;
  };
  attachment?: {
    name: string;
    meta: string;
    kind: "document" | "image" | "audio" | "video";
  };
  lifecycle: "active" | "recalled";
}

export interface SourceCandidate {
  sourceId: string;
  displayPath: string;
  clientVersion?: string;
  capability: "supported" | "probe_required" | "unsupported";
  databases: Array<{
    kind: string;
    encrypted: boolean;
    pageSizeHint?: number;
  }>;
}

export interface BootstrapState {
  portableRoot: string;
  portableRootWritable: boolean;
  sourceKeySaved: boolean;
  automaticRefresh: boolean;
  runtimeNetworkEnabled: boolean;
  implementationStage: string;
  enterpriseMode: boolean;
  organizationName?: string;
  collectionNotice?: string;
}

export interface FilterState {
  search: string;
  date: string;
  participant: string;
  type: MessageType | "all";
  mediaOnly: boolean;
}

export type ExportScope = "current_conversation" | "current_filter" | "entire_archive";
export type ExportFormat = "json" | "csv" | "html" | "pdf";
