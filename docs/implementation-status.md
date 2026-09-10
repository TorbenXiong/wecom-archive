# 实施状态

状态含义：`planned` 尚未实现，`scaffolded` 已有接口/UI 骨架，`implemented` 已完成代码，`verified` 已通过对应验收。

| 产品 | 工作项 | 状态 | 验证证据 |
| --- | --- | --- | --- |
| 普通版 | 居中采集/多格式导出 UI | implemented | JSON/CSV/HTML/TXT 选择、取消、失败恢复已通过组件测试；原生窗口验收待完成 |
| 客户端 | Windows 数据发现与一致性快照 | verified | 合成明文源端到端测试通过 |
| 客户端 | DPAPI CurrentUser | implemented | 待 Windows 轮换/清除测试 |
| 客户端 | 签名验证与隔离密钥探针 | verified | x86/x64 只读扫描、Authenticode/版本哈希校验、匿名管道和动态页头 raw wxSQLite3 key 校验；已验证密钥路径采用页头匹配后首个候选早停，并缓存签名校验结果；本机 5.0.10.6025 脱敏黑盒探针+采集测试 9.12 秒命中 1 个已验证候选 |
| 客户端 | SQLite3MultipleCiphers 静态链接 | implemented | v2.5.1 / SQLite 3.53.4；官方发布包 SHA-256 已固定并记录 |
| 客户端 | AES-128/AES-256-CBC 密钥读取 | verified | 口令与已派生 16/32 字节密钥桥接；raw wxSQLite3 页级派生、页头快速校验和整库解密；错误密钥拒绝、结构校验、DPAPI WCAK2 保存；合成夹具及本机脱敏端到端采集通过 |
| 共享 | `MessageV1` / `ArchiveBatchV1` / `ClientExportV1` | verified | Cargo 契约与篡改测试通过 |
| 共享 | 普通版可读 JSON/CSV/HTML/TXT writer | verified | 离线 Rust 测试覆盖联系人/会话名称映射、控制字符清理、按会话/参与人/发送者组织、移除机器字段、转义、注入防护、中文/媒体引用、空数据、校验和、并发不覆盖及失败清理 |
| 企业版 | 产品命名与加密收集规划 | implemented | 登录页、工作台、设置页使用企业版名称；规划流程与未开放状态已有组件测试 |
| 企业版 | 加密配置、员工收集端生成与密文导入解密 | planned | 见 enterprise-collection-plan.md；尚无可用加密功能 |
| 服务端 | 旧版 JSON 校验与导入 API | verified | 作为内部兼容实现保留，不在企业版产品界面开放；6 组认证/校验/冲突/幂等/导出测试通过 |
| 服务端 | 归档 schema / FTS / 幂等修订 | verified | 修订、缺失媒体与 v1→v2 迁移测试通过 |
| 服务端 | 三栏会话工作台与单文件静态资源 | implemented | 真实 API、内存令牌和筛选已有组件测试；旧版 JSON 导入入口已移除，前端资源嵌入服务端 exe |
| 服务端 | HTML/PDF/ZIP 富导出 | implemented | JSON/ZIP API 测试通过；PDF/Poppler 视觉验收待完成 |
| 服务端 | OIDC / RBAC | planned | 静态令牌仅限首期受控单节点 |
| 服务端 | retention / legal hold 执行器 | planned | schema 与契约已预留 |
| 全局 | 50 万消息性能 | planned | 待基准测试 |
