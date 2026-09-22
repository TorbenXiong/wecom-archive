import { CalendarClock, Download, MessagesSquare, ServerCog, Settings } from "lucide-react";

type Section = "conversations" | "local-export" | "server-config" | "collection-schedules" | "settings";

interface NavigationProps {
  active: Section;
  onSelect: (section: Section) => void;
}

const items: Array<{ id: Section; label: string; icon: typeof MessagesSquare }> = [
  { id: "local-export", label: "本机", icon: Download },
  { id: "server-config", label: "采集端", icon: ServerCog },
  { id: "conversations", label: "会话", icon: MessagesSquare },
  { id: "collection-schedules", label: "采集计划", icon: CalendarClock },
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
      <button
        className={active === "settings" ? "nav-item nav-settings active" : "nav-item nav-settings"}
        onClick={() => onSelect("settings")}
        type="button"
      >
        <Settings size={19} strokeWidth={1.9} />
        <span>设置</span>
      </button>
    </nav>
  );
}
