# 客户端 / 服务端架构

```text
Windows 客户端                                  中央服务端
┌──────────────────────────┐                    ┌──────────────────────────┐
│ 发现 / 探针 / DPAPI       │                    │ 身份校验 / JSON 导入      │
│ DB+WAL+SHM 快照           │── client-export ─▶│ 幂等归档 / 修订 / FTS5    │
│ MessageV1 / 媒体元数据    │                    │ 会话浏览 / 审计 / 富导出  │
│ 单一 JSON 导出入口        │                    │ JSON / CSV / HTML / PDF   │
└──────────────────────────┘                    └──────────────────────────┘
```

两端通过版本化 `MessageV1`、`ArchiveBatchV1` 和 `ClientExportV1` 交互。客户端不包含浏览工作台、全文索引或 HTML/PDF 生成器；服务端不接触员工终端源目录和密钥。

## 客户端链路

React 仅调用白名单 Tauri commands。`source-windows` 负责默认目录/手选目录探测、可信进程隔离探针、DPAPI 和 DB/WAL/SHM 一致性快照；`parser` 生成 `MessageV1`；`media-store` 记录 SHA-256 媒体元数据；`transfer` 生成 JSON（并保留内部兼容 CSV writer）。

客户端在 exe 同级 `userData/` 保存 DPAPI 密钥、脱敏诊断和临时工作区。导出时系统目录选择器默认打开 `userData/exports/`，最终写入用户选择的目录。源文件始终只读；快照不一致时返回可恢复错误并清理临时目录。程序目录不可写时返回 `PORTABLE_ROOT_NOT_WRITABLE`，不静默回退 `%LOCALAPPDATA%`。

## 服务端链路

服务端将网页资源嵌入 `WeComArchiveServer.exe`，默认仅监听 `127.0.0.1:8787`。导入顺序为：请求体限制 → 身份校验 → schema/关系校验 → 计数与 SHA-256 校验 → 按源消息 ID 幂等合并 → 导入审计。SQLite/FTS5 提供会话、参与人、消息、修订、媒体和审计查询。

## `ClientExportV1`

JSON 固定包含 schema 版本、导出 ID、客户端版本、批次授权/同意证据、采集范围、游标、消息/媒体计数和确定性 SHA-256。媒体首期只携带名称、MIME、大小、内容哈希及缺失原因，不写入本地绝对路径或二进制。CSV 是人工查看的扁平格式，不支持回灌。

## 生命周期

1. 自动发现并隔离读取源数据，复制一致性快照。
2. 验证候选密钥和数据库结构，归一化消息并关联媒体。
3. 生成 JSON `.partial`，成功后原子改名并允许用户导入服务端。
4. 服务端校验后幂等入库，变化内容写入新 revision，不覆盖历史证据。

未来上传可采用每批随机 DEK、AES-256-GCM 分块、服务端公钥封装和幂等续传；默认构建不开放网络 capability。
