import { Archive, Building2, Minus, Square, X } from "lucide-react";

export function AppHeader() {
  return (
    <header className="app-header">
      <div className="brand-lockup">
        <span className="brand-mark" aria-hidden="true"><Archive size={18} strokeWidth={2.2} /></span>
        <span className="brand-name">企微归档</span>
        <span className="mode-badge">企业版</span>
        <span className="privacy-note"><Building2 size={15} />企业内网自部署</span>
      </div>
      <div className="window-controls" aria-hidden="true">
        <span><Minus size={16} /></span><span><Square size={13} /></span><span><X size={16} /></span>
      </div>
    </header>
  );
}
