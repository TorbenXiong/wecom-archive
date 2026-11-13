# 开发与构建

[返回首页](../README.md) · [文档目录](README.md)

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

详细设计见 [架构](architecture.md)、[安全边界](privacy-security.md)、[产品能力](capability-matrix.md)。

采集范围和已知限制见[本机采集说明](local-collection-fixes.md)。

合成数据性能基准见 [`docs/benchmarks.md`](benchmarks.md)，使用 PowerShell 运行 `./scripts/benchmark-archive.ps1`。

会话消息相邻翻页与定位后的取页使用消息锚点，随机页码及失效锚点保留 OFFSET；兼容性、存储层对照和限制见 [深分页与消息定位](message-pagination.md)。

消息图片按需读取，点击预览复用已读取内容；资源生命周期与兼容限制见[图片加载](architecture.md#图片加载)。

大包内存分配对照见 [`docs/package-memory.md`](package-memory.md)，覆盖 JSON 导入、加密包编码和密文字段解码；数据均为合成内容，默认离线，不使用真实密钥。

## 测试与构建

工具链：Rust `1.98.1`、Node.js `>=24`、pnpm `10.30.3`。版本与来源见[依赖计划](dependency-plan.md)。以下命令在仓库根目录执行；打包脚本使用 PowerShell。

```powershell
pnpm test
pnpm exec tsc -b
pnpm build:client
pnpm build:server
cargo test --workspace --locked --offline
cargo build --release --locked --offline --bin WeComArchive
```

`build:client` 生成采集端界面资源，`build:server` 生成归档工作台资源；采集端文件由服务端配置页按需生成。
前端构建完成后再编译 Rust，避免嵌入旧资源。以上命令要求工具链和依赖已在本机准备好；`--offline` 不下载缺失依赖。

## 打包

使用 PowerShell 运行：

```powershell
./scripts/package-release.ps1
```

打包入口执行测试、类型与格式检查、Clippy、前端和 Windows Release 构建及隔离桌面检查。真实环境检查需用户明确授权后添加 `-IncludeRealEnvironment`。

`-PackageName` 指定全新交付目录，`-Cargo`、`-Node` 指定本机工具。交付目录仅含一个 `WeComArchive.exe`，打包后检查版本、x64 架构和哈希，不覆盖旧包。
