# 企业微信记录归档

独立实现的 Windows 企业微信归档工具。发布包只有一个 `WeComArchive.exe`：正常启动进入归档工作台，可直接采集本机数据；工作台也能生成供其他电脑使用的专属采集端。所有源数据库均通过只读快照解析，不修改企业微信数据。

## 功能

- 浏览、全局搜索和导出会话，展示成员名称、引用消息、群公告、图片和文件；搜索结果可直接定位到原消息。
- 在会话页直接采集本机数据，也可导入专属采集端生成的 `.wca` 文件。
- 配置服务端 URL、访问令牌、加密密钥、员工告知、媒体采集和数据脱敏，然后生成单文件 Windows 采集端。
- 采集端按用户操作上传到指定的已认证工作台；归档按源消息 ID 增量合并多个采集端的数据。
- 导出 JSON、CSV、HTML 或 PDF 单文件，默认启用数据脱敏、简化信息和美化信息；关闭美化时输出紧凑格式。
- 简化导出保留会话、发送人、时间、消息类型、正文和附件名称，移除系统 ID、原始数据和群目录。简化 JSON 仅供阅读，完整 JSON 可重新导入。
- 媒体按内容哈希流式采集，图片可预览，文件通过 Windows 默认应用打开；不生成 ZIP 或校验旁车文件。

`WeComArchive.exe` 在同一进程内运行桌面窗口和本机归档服务，默认监听 `127.0.0.1:8787`；关闭桌面窗口时服务同步退出。生成专属采集端时，程序复制自身并附加签名配置，因此生成结果仍是同一程序的采集端运行模式，不是另一产品版本。

## 工程结构

```text
apps/server/          归档 API 与桌面工作台服务
src/                  React 工作台与采集端界面
src-tauri/            Windows 桌面宿主和白名单 commands
crates/domain/        版本化归档契约
crates/source-windows 数据发现、快照、探针和 DPAPI
crates/parser/        消息归一化
crates/transfer/      采集传输与数据脱敏
crates/archive-store/ SQLite/FTS5 归档、修订和审计
crates/export/        JSON/CSV/HTML/PDF 导出
```

详细设计见 [`docs/architecture.md`](docs/architecture.md)，安全边界见 [`docs/privacy-security.md`](docs/privacy-security.md)，当前能力见 [`docs/capability-matrix.md`](docs/capability-matrix.md)，发布前状态见 [`docs/implementation-status.md`](docs/implementation-status.md)。

## 开发

工具链：Rust `1.98.1`、Node.js `>=24`、pnpm `10.30.3`。版本与来源见 [`docs/dependency-plan.md`](docs/dependency-plan.md)。

```powershell
pnpm test
pnpm exec tsc -b
pnpm build:client
pnpm build:server
cargo test --workspace --locked
cargo build --release --locked --bin WeComArchive
```

`build:client` 生成专属采集端界面资源，`build:server` 生成归档工作台资源；它们最终都进入同一个 `WeComArchive.exe`。

## 运行数据

- 工作台使用 exe 同级 `serverData/` 保存访问令牌、加密私钥、归档库、媒体、采集端和导出文件。
- 本机采集和专属采集端均使用系统临时目录处理快照，完成后清理。
- 专属采集端每次从自身签名配置读取 URL、令牌、密钥和采集选项，不在 exe 同级创建 `userData`。

## 安全边界

工作台默认仅监听本机。专属采集端仅在用户点击上传后连接配置的服务端 URL；外网地址应使用 HTTPS。程序不包含遥测、更新检查、远程字体或第三方资源。日志仅记录阶段、计数、哈希前缀和稳定错误码，不记录聊天正文、凭据、密钥或私人路径。

## License

MIT License. Copyright (c) 2026 Torben Xiong.
