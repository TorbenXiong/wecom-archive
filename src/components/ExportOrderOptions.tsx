import type { ExportOrder } from "../lib/server-api";

interface ExportOrderOptionsProps {
  conversationOrder: ExportOrder;
  messageOrder: ExportOrder;
  onConversationOrderChange: (order: ExportOrder) => void;
  onMessageOrderChange: (order: ExportOrder) => void;
  disabled?: boolean;
}

export function ExportOrderOptions({ conversationOrder, messageOrder, onConversationOrderChange, onMessageOrderChange, disabled }: ExportOrderOptionsProps) {
  return <fieldset className="export-order-options">
    <legend>排序方式</legend>
    <label><span>会话时间</span><select aria-label="会话时间排序" disabled={disabled} value={conversationOrder} onChange={(event) => onConversationOrderChange(event.target.value as ExportOrder)}><option value="ascending">升序（较早会话在前）</option><option value="descending">降序（最近会话在前）</option></select></label>
    <label><span>会话内容时间</span><select aria-label="会话内容时间排序" disabled={disabled} value={messageOrder} onChange={(event) => onMessageOrderChange(event.target.value as ExportOrder)}><option value="ascending">升序（较早内容在前）</option><option value="descending">降序（最近内容在前）</option></select></label>
  </fieldset>;
}
