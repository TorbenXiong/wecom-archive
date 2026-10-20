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
  isGroup: boolean;
  kind?: "document" | "application" | "assistant" | "temporary" | "system";
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
  rawType?: string;
  body?: string;
  richSegments?: Array<{ kind: "text" | "emoji"; text: string }>;
  quote?: {
    id: string;
    sender: string;
    time: string;
    body: string;
  };
  attachment?: {
    name: string;
    meta: string;
    kind: "document" | "image" | "audio" | "video";
    contentHash?: string;
  };
  lifecycle: "active" | "recalled";
}

export interface ParticipantItem {
  id: string;
  name: string;
  kind?: string;
  profile?: ParticipantProfile;
}

export interface ParticipantProfile {
  [key: string]: string | undefined;
  mobile?: string;
  phone?: string;
  office_phone?: string;
  email?: string;
  employee_no?: string;
  department?: string;
  organization?: string;
  position?: string;
  address?: string;
  english_name?: string;
  signature?: string;
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
  organizationName?: string;
  displayName?: string;
  offlineExportEnabled?: boolean;
  hiddenModeEnabled?: boolean;
  collectorSchedule?: CollectionSchedule;
}

export type CollectionScheduleMode = "disabled" | "interval" | "daily";

export interface CollectionSchedule {
  mode: CollectionScheduleMode;
  intervalMinutes: number;
  dailyTime: string;
}

export interface FilterState {
  search: string;
  date: string;
  participant: string;
  type: MessageType | "all";
  mediaOnly: boolean;
}

export type ExportScope = "current_conversation" | "current_filter" | "selected_conversations" | "date_range" | "entire_archive";
export type ExportFormat = "json" | "csv" | "html" | "md";
