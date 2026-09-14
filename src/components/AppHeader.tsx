import { Archive, Building2 } from "lucide-react";

export function AppHeader() {
  return (
    <header className="app-header">
      <div className="brand-lockup">
        <span className="brand-mark" aria-hidden="true"><Archive size={18} strokeWidth={2.2} /></span>
        <span className="brand-name">企微归档</span>
        <span className="mode-badge">企业版</span>
        <span className="privacy-note"><Building2 size={15} />企业内网自部署</span>
      </div>
    </header>
  );
}
