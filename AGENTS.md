# 项目工作约定

- 本项目必须保持独立实现，不复制、改编或依赖任何同类项目的源码、目录、命名或资源。
- Windows 采集客户端运行时默认离线；未经产品层显式配置、员工授权和用户启用，不得增加网络请求。服务端网络能力仅限已认证的本产品 API，不得加入遥测、远程字体或第三方资源。
- WebView 只能调用白名单 Tauri commands，不得开放通用 shell、进程、文件系统或任意路径访问。
- 源数据只读：先复制数据库、WAL、SHM 到 `userData/work/<run-id>`，再进行校验和读取。
- 日志、错误和测试产物必须脱敏；禁止提交真实聊天、账号、密钥、私人路径或截图。
- `MessageV1`、`ArchiveBatchV1`、`ClientExportV1`、`UploadEnvelopeV1` 属于版本化契约。修改字段语义或删除字段必须先说明迁移影响。
- 导出内容必须转义、校验范围、防路径穿越；CSV 必须防公式注入；所有导出经 `.partial` 原子完成且不覆盖已有文件。
- Windows 特定代码放在 `crates/source-windows`；跨平台 domain、archive、parser、media、export 不得依赖 Windows API。
- 新增依赖或更改锁文件前遵守全局下载安装确认规则。

## 交付打包约定

- Windows 工作台交付包必须使用 `artifacts/<包目录>/` 目录形式，目录初始内容只允许一个 `WeComArchive.exe` 单程序。
- 交付目录不得包含 ZIP、SHA-256 校验文件、说明文档、源码、`serverData/`、`collectorData/` 或其他旁车文件；`serverData/` 和 `collectorData/` 只能在程序首次运行后由程序生成，不属于打包内容。
- 生成包前先完成前端资源构建和 Windows x64 Release 构建，再将最终 `target/release/WeComArchive.exe` 复制到交付目录；不得从旧 `artifacts` 子目录拼装或覆盖正在运行的工作台。
- 打包后必须检查交付目录的文件清单、EXE 版本号、Windows x64 架构和文件哈希；清单不符合“仅一个 `WeComArchive.exe`”时不得交付。
