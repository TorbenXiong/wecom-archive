# 企业微信记录归档

独立实现的普通版 Windows 本地归档工具与企业版中央归档服务。普通版供个人单独使用，负责发现、解密、归一化和本地导出；企业版未来通过自己生成的专属采集端接收加密数据，再提供浏览、检索、审计和富格式导出。普通版运行时默认离线，不读取或修改源文件。

## 产品

### 普通版（Windows 客户端）

- 启动后自动发现本机企业微信数据，使用隔离探针和 DPAPI 完成密钥处理。
- 先复制 DB/WAL/SHM 快照，再归一化为 `MessageV1` 并关联媒体元数据。
- 页面随窗口水平、垂直居中，小窗口内容超出时可滚动；选择格式后通过单一“导出”入口保存。
- 支持 JSON、CSV、HTML、TXT：JSON 按会话、参与人和消息组织，CSV 用于表格查看，HTML 用于离线阅读/打印，TXT 用于纯文本检索。
- 当前所有格式仅含媒体引用，不打包媒体文件，也不用于企业版导入。
- 导出时默认定位到 exe 同级 `userData/exports/`，用户可改选目录；文件以 `.partial` 原子生成且不覆盖已有文件。

### 企业版（中央归档服务端）

- 当前提供中央归档浏览、检索、审计和富格式导出能力。
- 提供会话浏览、全文检索、组合筛选、媒体完整性和 JSON/CSV/HTML/PDF/ZIP 导出。
- 单节点首期默认监听 `127.0.0.1:8787`，网页资源编译进单一 `WeComArchiveServer.exe`。
- 加密配置、生成员工收集端、加密数据导入解密属于后续规划，当前尚未开放。详见 [`docs/enterprise-collection-plan.md`](docs/enterprise-collection-plan.md)。

## 工程结构

```text
apps/server/          服务端 API 与工作台
src/                  React 客户端/服务端界面
src-tauri/            Windows 客户端壳与 Tauri commands
crates/domain/        MessageV1、ArchiveBatchV1、ClientExportV1
crates/source-windows 数据发现、快照、探针与 DPAPI
crates/parser/        消息归一化
crates/transfer/      普通版可读 JSON/CSV/HTML/TXT 写入与旧版归档 JSON 校验
crates/archive-store/ 服务端 SQLite、FTS5、修订与审计
crates/export/        服务端 JSON/CSV/HTML/PDF/ZIP 导出
```

详细架构见 [`docs/architecture.md`](docs/architecture.md)，能力范围见 [`docs/capability-matrix.md`](docs/capability-matrix.md)，当前状态见 [`docs/implementation-status.md`](docs/implementation-status.md)。

## 开发

工具链：Rust `1.98.1`、Node.js `>=24`、pnpm `10.30.3`。依赖版本和来源记录在 [`docs/dependency-plan.md`](docs/dependency-plan.md)。

```powershell
pnpm dev:client
pnpm dev:server
pnpm build:client
pnpm build:server
cargo test --workspace --locked
```

服务端示例（PowerShell）：

```powershell
$env:WECOM_ARCHIVE_SERVER_TOKEN = "请替换为至少24字节的随机令牌"
cargo run -p wecom-archive-server --locked
```

浏览器打开 `http://127.0.0.1:8787`，输入同一令牌后进入企业版工作台。

## 安全边界

仅处理数据主体有权归档的数据。客户端不访问网络、不上传遥测；日志仅保留版本、阶段、计数、哈希前缀和错误码。真实聊天、账号、密钥、私人路径和截图不得进入仓库。未来上传能力必须独立配置、明确授权后启用。

## License

MIT License. Copyright (c) 2026 Torben Xiong.
