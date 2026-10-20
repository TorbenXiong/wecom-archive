import { Building2, BriefcaseBusiness, FileText, Mail, MapPin, Phone, ShieldCheck, UserRound, X } from "lucide-react";
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
          <div>
            <h2 id="participant-profile-name">{participant.name}</h2>
            {participant.profile?.organization && <p className="participant-profile-organization">{participant.profile.organization}</p>}
            {participant.profile?.signature && <p className="participant-profile-signature">{participant.profile.signature}</p>}
          </div>
        </div>
        <dl className="participant-profile-meta">
          {participant.profile?.department && <div><dt><Building2 size={14} />部门</dt><dd>{participant.profile.department}</dd></div>}
          {participant.profile?.position && <div><dt><BriefcaseBusiness size={14} />职务</dt><dd>{participant.profile.position}</dd></div>}
          {participant.profile?.email && <div><dt><Mail size={14} />邮箱</dt><dd>{participant.profile.email}</dd></div>}
          {participant.profile?.employee_no && <div><dt><FileText size={14} />工号</dt><dd>{participant.profile.employee_no}</dd></div>}
          {participant.profile?.mobile && <div><dt><Phone size={14} />手机</dt><dd>{participant.profile.mobile}</dd></div>}
          {participant.profile?.phone && <div><dt><Phone size={14} />电话</dt><dd>{participant.profile.phone}</dd></div>}
          {participant.profile?.office_phone && <div><dt><Phone size={14} />座机</dt><dd>{participant.profile.office_phone}</dd></div>}
          {participant.profile?.address && <div><dt><MapPin size={14} />地址</dt><dd>{participant.profile.address}</dd></div>}
          {participant.profile?.english_name && <div><dt><UserRound size={14} />英文名</dt><dd>{participant.profile.english_name}</dd></div>}
          {Object.entries(participant.profile || {}).filter(([key, value]) => key.startsWith("custom:") && value?.trim()).map(([key, value]) => <div key={key}><dt><FileText size={14} />{key.slice(7)}</dt><dd>{value}</dd></div>)}
          <div><dt><ShieldCheck size={14} />归档标识</dt><dd title={participant.id}>{shortIdentifier(participant.id)}</dd></div>
          <div><dt><BriefcaseBusiness size={14} />身份</dt><dd>{kind}</dd></div>
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
