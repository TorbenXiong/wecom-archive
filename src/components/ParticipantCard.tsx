import { Building2, ShieldCheck, UserRound, X } from "lucide-react";
import type { ParticipantItem } from "../domain/types";

interface ParticipantCardProps {
  participant: ParticipantItem;
  onClose: () => void;
}

export function ParticipantCard({ participant, onClose }: ParticipantCardProps) {
  const kind = participantKindLabel(participant.kind);
  return (
    <div className="participant-card-backdrop" role="presentation" onMouseDown={onClose}>
      <section className="participant-profile-card" role="dialog" aria-modal="true" aria-labelledby="participant-profile-name" onMouseDown={(event) => event.stopPropagation()}>
        <button className="participant-profile-close" type="button" aria-label="关闭人员详情" onClick={onClose}><X size={18} /></button>
        <div className="participant-profile-head">
          <span className="participant-profile-avatar">{Array.from(participant.name).at(-1) || "员"}</span>
          <div><h2 id="participant-profile-name">{participant.name}</h2><p><Building2 size={14} />企业微信成员</p></div>
        </div>
        <dl className="participant-profile-meta">
          <div><dt><UserRound size={14} />身份</dt><dd>{kind}</dd></div>
          <div><dt><ShieldCheck size={14} />归档标识</dt><dd title={participant.id}>{shortIdentifier(participant.id)}</dd></div>
        </dl>
      </section>
    </div>
  );
}

function participantKindLabel(kind?: string): string {
  if (!kind) return "会话成员";
  const labels: Record<string, string> = { employee: "企业员工", external: "外部联系人", group: "群成员", bot: "机器人" };
  return labels[kind] || kind;
}

function shortIdentifier(value: string): string {
  return value.length <= 18 ? value : `${value.slice(0, 8)}…${value.slice(-6)}`;
}
