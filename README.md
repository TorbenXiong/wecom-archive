# 企业微信记录归档

独立实现的 Windows 企业微信归档工具。发布包只有一个 `WeComArchive.exe`：正常启动进入归档工作台，可直接采集本机数据；工作台也能生成供其他电脑使用的专属采集端。所有源数据库均通过只读快照解析，不修改企业微信数据。

## 功能

- 浏览、全局搜索和导出会话，展示成员名称、引用消息、群公告、图片和文件；搜索结果可直接定位到原消息。
- 立即采集本机文本或完整内容，也可导入专属采集端生成的 `.wca` 文件。
- 独立维护多条本机采集计划，并查看已生成采集端携带的固定间隔或每日定时计划及运行状态。
- 配置服务端、凭据、员工告知、采集范围和脱敏策略后生成单文件 Windows 采集端；采集结果按源消息 ID 增量、幂等合并。
- 导出时可分别设置会话和会话内容的升降序；界面在全量消息排序后分页。
- 导出 JSON、CSV、HTML 或 Markdown 单文件，默认启用数据脱敏、简化信息和美化信息；阅读型 JSON、HTML 和 Markdown 按会话分组，CSV 按会话连续输出；关闭美化时输出紧凑格式。
- 简化导出保留会话、发送人、时间、消息类型、正文和附件名称，移除系统 ID、原始数据和群目录。简化 JSON 仅供阅读，完整 JSON 可重新导入。
- 媒体按内容哈希流式采集，图片可预览，文件通过 Windows 默认应用打开；不生成 ZIP 或校验旁车文件。

`WeComArchive.exe` 在同一进程内运行桌面窗口和本机归档服务，默认监听 `127.0.0.1:9812`。窗口可隐藏并按计划后台运行；退出程序会停止服务和任务。专属采集端是附加签名配置后的同一程序，不是另一产品版本。

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
crates/export/        JSON/CSV/HTML/Markdown 导出
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
- 专属采集端从自身签名配置读取服务端、凭据、采集选项和计划，不在 exe 同级创建 `userData`；配置变更需重新生成采集端。

## 安全边界

工作台默认仅监听本机。专属采集端仅在用户点击上传或显式启用签名配置中的后台计划后连接配置的服务端 URL；外网地址应使用 HTTPS。程序不包含遥测、更新检查、远程字体或第三方资源。日志仅记录阶段、计数、哈希前缀和稳定错误码，不记录聊天正文、凭据、密钥或私人路径。

## License

MIT License. Copyright (c) 2026 Torben Xiong.
