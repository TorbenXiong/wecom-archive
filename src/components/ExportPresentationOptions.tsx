interface ExportPresentationOptionsProps {
  dataRedaction: boolean;
  simplify: boolean;
  pretty: boolean;
  onDataRedactionChange: (enabled: boolean) => void;
  onSimplifyChange: (enabled: boolean) => void;
  onPrettyChange: (enabled: boolean) => void;
  disabled?: boolean;
}

export function ExportPresentationOptions({ dataRedaction, simplify, pretty, onDataRedactionChange, onSimplifyChange, onPrettyChange, disabled }: ExportPresentationOptionsProps) {
  return <div className="export-presentation-options">
    <label className="export-data-redaction-option" title="导出前脱敏账号、密码、令牌、联系方式及疑似凭据片段；文件名和图片名保持原样。"><input type="checkbox" checked={dataRedaction} disabled={disabled} onChange={(event) => onDataRedactionChange(event.target.checked)} />数据脱敏</label>
    <label className="export-data-redaction-option" title="保留会话、发送人、时间、正文和附件名称，去掉系统 ID、群成员、群公告等信息。简化 JSON 仅供阅读，不能重新导入。"><input type="checkbox" checked={simplify} disabled={disabled} onChange={(event) => onSimplifyChange(event.target.checked)} />简化信息</label>
    <label className="export-data-redaction-option" title="开启后使用易读排版；关闭时紧凑输出，保留正文换行及 CSV 必需分行。"><input type="checkbox" checked={pretty} disabled={disabled} onChange={(event) => onPrettyChange(event.target.checked)} />美化信息</label>
  </div>;
}
