# Changelog

## 0.4.0

- 连接配置新增可选单层 `group` 和多个 `tags`，兼容旧配置，标签去除首尾空白、空项与重复值。
- `list` 新增 `--group`、`--tag` 精确筛选，同时传入时取交集；保留 `configured` 名称数组，新增 `connections` 展示元数据，不输出 URL 或凭据。
- 编辑页面支持「全部 / 按分组 / 按标签」展示、分类折叠、搜索和分组/标签编辑，多标签入口编辑同一连接。
- 每轮列表渲染按连接复用脏状态和错误状态，减少多标签展示的重复计算。
- 标签删除改用归一化数组索引，修复含 CR/CRLF 的标签经 HTML 属性转换后无法删除的问题。
- 同步中英文 README、示例配置及两份内置 Agent SKILL；清理 npm lockfile 中仍指向旧版本平台产物的记录。
- 验证：43 项测试通过，包含真实 Chrome 分类展示、标签增删、CRLF/LF 标签分别删除、保存重读与清空标签；release 构建、版本一致性、CLI 筛选冒烟和格式检查通过。

## 0.3.0

- 新增 `editor` 命令：`editor start` 启动本地编辑服务并打开页面，`editor stop` 关闭，`editor status` 查看状态；服务只监听 `127.0.0.1`，`/api/*` 需要一次性访问令牌。
- 页面支持按数据库类型可视化编辑连接配置、实时预览将要写入的 JSON、就地编辑 JSON 文本，并展示本机到数据库的访问路径（含 SSH 跳板）。
- 保存走完整校验：JSON 结构、配置规则通过后先备份 `config.json.bak`，再以临时文件加 rename 原子替换，并保留原文件权限。
- 页面只读写配置文件原文，不触发明文密码迁移；`passwordRef` / `passphraseRef` 默认显示为已加密，点眼睛才按需解密，解密结果只用于显示、不写回配置文件。
- 右侧 JSON 面板可与磁盘内容做行级 diff，只在边栏标出新增行和修改行，保存后标记清空。
- 页面源码按 `core / api / render / actions / boot` 拆到 `rust/assets/editor/`，由 `build.rs` 拼成单文件 HTML 后内嵌，对外交付仍是纯 HTML，构建期不引入 JS 工具链。
- 新增两层页面回归防线：页面脚本静态自检（抓到过一次真实的「调用已删除函数」），以及基于 headless Chrome 的页面 smoke（密文显隐、URL 输入实时同步、JSON 面板范围）。
- 编辑服务新增空闲自动退出：超过 `10` 分钟没有操作会自己退出并清理状态文件；页面在有操作时按分钟打点保活，临近超时会提示。
- 页面在服务已退出时给出可照做的报错（提示重新执行 `editor start`），不再只抛 `Failed to fetch`；心跳失败也会提醒一次，避免继续编辑却保存不进去。
- 修复「已加密」状态胶囊里图标与文字没有垂直居中（胶囊高度由行盒改为固定高度 + 弹性居中），并加了对应的布局回归断言。
- 编辑服务状态文件支持 `AGENT_DATABASE_CLI_EDITOR_STATE_DIR` 覆盖目录，仅用于测试隔离。

## 0.2.19
- 安全优化：数据库 URL 明文密码、SSH 隧道密码和私钥口令在首次使用连接时自动迁移到本地加密存储，配置文件仅保留 `passwordRef` / `passphraseRef`。
- 兼容性优化：保留明文字段改密入口，重新填写明文密码后下次使用会覆盖旧密文。

## 0.2.18

- Bugfix: `daemon status` 在 daemon 未运行或 Unix socket 残留时返回明确的未运行状态，避免直接暴露 `Connection refused`。

## 0.2.17

- 性能优化：daemon 数据库请求不再持有全局配置锁，改为按数据库连接粒度串行执行；不同数据库的慢查询不会互相阻塞。
- 稳定性优化：daemon 首次初始化同一数据库连接时增加初始化占位，避免并发冷启动重复创建 SSH 隧道和数据库连接。
- 安全优化：Redis keys 元信息查询改用 `SCAN` 分批读取，避免使用阻塞式 `KEYS`；只读模式下 Redis `KEYS` 命令会被拒绝。
- 安全优化：只读模式额外拒绝 PostgreSQL `SELECT INTO`、CTE 后接写操作，以及 MongoDB aggregate `$out` / `$merge` 等具备写入语义的查询。
- 易用性优化：daemon 空响应时返回明确错误信息，避免暴露底层 `EOF while parsing a value`。
- 质量优化：清理 clippy 报出的冗余代码写法，保持 `cargo fmt`、`cargo test`、`cargo clippy --all-targets -- -D warnings` 通过。

## 0.2.16

- Bugfix: 修复 Windows 下 daemon named pipe 客户端和服务端未实现，导致 `test`、`exec`、`meta`、`reset`、`daemon status` 等命令无法正常通过本地 daemon 工作的问题。
