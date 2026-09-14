import { MessagesSquare, ServerCog, Settings, SquareArrowOutUpRight } from "lucide-react";

type Section = "conversations" | "exports" | "server-config" | "settings";

interface NavigationProps {
  active: Section;
  onSelect: (section: Section) => void;
}

const items: Array<{ id: Section; label: string; icon: typeof MessagesSquare }> = [
  { id: "conversations", label: "会话", icon: MessagesSquare },
  { id: "exports", label: "导出记录", icon: SquareArrowOutUpRight },
  { id: "server-config", label: "服务端配置", icon: ServerCog },
  { id: "settings", label: "设置", icon: Settings },
];

export function Navigation({ active, onSelect }: NavigationProps) {
  return (
    <nav className="primary-nav" aria-label="主导航">
      {items.map(({ id, label, icon: Icon }) => (
        <button
          className={active === id ? "nav-item active" : "nav-item"}
          key={id}
          onClick={() => onSelect(id)}
          type="button"
        >
          <Icon size={19} strokeWidth={1.9} />
          <span>{label}</span>
        </button>
      ))}
      <div className="nav-spacer" />
      <div className="nav-security"><span />离线运行</div>
    </nav>
  );
}
