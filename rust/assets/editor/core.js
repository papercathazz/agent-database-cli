  /* ==========================================================
     常量
     ========================================================== */
  const TYPES = [
    { id: "mysql", label: "mysql", hint: "mysql://user@127.0.0.1:3306/app" },
    { id: "postgres", label: "postgres", hint: "postgres://user@127.0.0.1:5432/app?sslmode=require" },
    { id: "redis", label: "redis", hint: "redis://127.0.0.1:6379" },
    { id: "oracle", label: "oracle", hint: "oracle://USER@127.0.0.1:1521/service" },
    { id: "mongodb", label: "mongodb", hint: "mongodb://user@127.0.0.1:27017/app" }
  ];
  const ORACLE_DRIVERS = ["sqlcl", "oracledb"];
  const SSH_MODES = [
    { id: "password", label: "密码" },
    { id: "path", label: "私钥文件" },
    { id: "inline", label: "私钥内容" }
  ];
  const TOKEN = new URLSearchParams(location.hash.slice(1)).get("t") || "";
  const $ = (sel, root) => (root || document).querySelector(sel);
  const $$ = (sel, root) => Array.from((root || document).querySelectorAll(sel));
  const icon = (name, cls) => '<svg class="i ' + (cls || "") + '"><use href="#i-' + name + '"></use></svg>';
  const esc = (value) => String(value).replace(/[&<>"]/g, (ch) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[ch]));

  /* ==========================================================
     状态
     ========================================================== */
  const S = {
    path: "",
    data: { databases: {} },
    issues: [],
    selected: null,
    // 磁盘内容的快照，脏标记和行级 diff 都从这里派生
    baselineData: { databases: {} },
    backup: null,
    busy: false,
    jsonOpen: false,
    textDirty: false,
    // 每个密文字段各自的显示状态：key 用字段名（url / sshTunnel.password / sshTunnel.passphrase）
    shown: {},
    railSignature: "",
    listMode: "all",
    search: "",
    collapsedSections: new Map(),
    // JSON 面板范围：one = 只显示当前连接，all = 整个配置文件
    scope: "one",
    // 点眼睛解密出来的明文，只在内存里按 ref 缓存
    secrets: {},
    // 后端下发的推荐黑名单，避免页面自己维护一份清单
    recommendedBlacklist: {},
    jsonError: null
  };

  /* ==========================================================
     数据工具
     ========================================================== */
  const clone = (value) => JSON.parse(JSON.stringify(value));
  const databases = () => S.data.databases || {};
  const current = () => databases()[S.selected] || {};

  // 分类展示与序列化共用归一化规则；错误类型留给服务端校验，不在页面静默纠正。
  function databaseGroup(db) {
    return typeof db.group === "string" ? db.group.trim() || null : null;
  }

  function databaseTags(db) {
    return Array.isArray(db.tags)
      ? [...new Set(db.tags.filter((tag) => typeof tag === "string").map((tag) => tag.trim()).filter(Boolean))]
      : [];
  }

  function connectionSections(entries, mode, query) {
    const search = (query || "").trim().toLowerCase();
    const sections = new Map();
    for (const [name, db] of Object.entries(entries)) {
      const tags = databaseTags(db);
      const category = databaseGroup(db);
      if (![name, db.type, db.url, category, ...tags].join(" ").toLowerCase().includes(search)) continue;
      const keys = mode === "tags" ? (tags.length ? tags : [null]) : mode === "group" ? [category] : [null];
      keys.forEach((key) => {
        if (!sections.has(key)) sections.set(key, []);
        sections.get(key).push(name);
      });
    }
    return Array.from(sections, ([key, names]) => ({ key, names }));
  }

  function getPath(root, path) {
    return path.split(".").reduce((node, key) => (node == null ? undefined : node[key]), root);
  }
  function setPath(root, path, value) {
    const keys = path.split(".");
    const last = keys.pop();
    let node = root;
    for (const key of keys) {
      if (typeof node[key] !== "object" || node[key] === null) node[key] = {};
      node = node[key];
    }
    if (value === undefined || value === null) delete node[last];
    else node[last] = value;
  }

  // 剔除空值，避免写出会被 CLI 校验拒绝的内容（例如 password: ""、nodes: []）
  // url 例外：它是必填字段，空值也要保留，这样 CLI 会给出带连接名的提示而不是 serde 的结构错误
  function cleanDatabase(source) {
    const db = clone(source);
    if (typeof db.group === "string") db.group = databaseGroup(db);
    if (Array.isArray(db.tags) && db.tags.every((tag) => typeof tag === "string")) db.tags = databaseTags(db);
    for (const key of Object.keys(db)) {
      if (key === "url") continue;
      const value = db[key];
      if (key === "group" && value !== null && typeof value !== "string") continue;
      if (key === "tags" && value !== null && (!Array.isArray(value) || !value.every((tag) => typeof tag === "string"))) continue;
      if (value === "" || value === null || (Array.isArray(value) && value.length === 0)) delete db[key];
    }
    if (db.sshTunnel && typeof db.sshTunnel === "object") {
      const tunnel = db.sshTunnel;
      for (const key of Object.keys(tunnel)) {
        const value = tunnel[key];
        if (value === "" || value === null || (Array.isArray(value) && value.length === 0)) delete tunnel[key];
      }
      if (Object.keys(tunnel).length === 0) delete db.sshTunnel;
    }
    if (db.redisCluster && typeof db.redisCluster === "object") {
      const cluster = db.redisCluster;
      cluster.nodes = (cluster.nodes || []).filter((node) => String(node || "").trim() !== "");
      // nodes 为空时整个 redisCluster 都必须移除，否则 CLI 解析会失败
      if (cluster.nodes.length === 0) delete db.redisCluster;
    }
    if (Array.isArray(db.blacklist)) {
      db.blacklist = db.blacklist.map((item) => String(item).trim()).filter(Boolean);
      if (db.blacklist.length === 0) delete db.blacklist;
    }
    return db;
  }

  // 序列化：默认序列化当前数据，也可以传入一份快照
  function serialize(data) {
    const source = data || S.data;
    const out = {};
    for (const [name, db] of Object.entries((source && source.databases) || {})) {
      out[name] = cleanDatabase(db);
    }
    return JSON.stringify({ databases: out }, null, 2);
  }

  // 面板范围：单个连接时只序列化该连接，键名就是连接名，方便直接复制进另一个配置
  function serializeScope() {
    if (S.scope === "one") {
      const name = S.selected;
      if (!name || !databases()[name]) return "{}";
      return JSON.stringify({ [name]: cleanDatabase(databases()[name]) }, null, 2);
    }
    return serialize();
  }

  const isDirty = () => serialize() !== serialize(S.baselineData);

  // 每个连接单独和基线比，轨道里才能只在真正改过的连接上打点
  function rowDirty(name) {
    const base = (S.baselineData.databases || {})[name];
    return JSON.stringify(cleanDatabase(databases()[name] || {})) !== JSON.stringify(cleanDatabase(base || {}));
  }

  // 面板当前范围对应的磁盘内容行，用来做行级 diff
  function baselineLines() {
    const base = S.baselineData.databases || {};
    if (S.scope === "one") {
      if (!S.selected || !base[S.selected]) return [];
      return JSON.stringify({ [S.selected]: cleanDatabase(base[S.selected]) }, null, 2).split("\n");
    }
    const all = {};
    for (const [name, db] of Object.entries(base)) all[name] = cleanDatabase(db);
    return JSON.stringify({ databases: all }, null, 2).split("\n");
  }

  // 行级 diff：先裁掉公共前后缀，只对中间差异区做 LCS；
  // 差异区超过阈值时整段标记，避免大配置下矩阵规模爆炸拖慢输入
  const DIFF_LIMIT = 400;

  function diffLines(base, current) {
    const marks = new Array(current.length).fill("");
    if (!current.length) return marks;
    let head = 0;
    while (head < base.length && head < current.length && base[head] === current[head]) head += 1;
    let tail = 0;
    while (
      tail < base.length - head
      && tail < current.length - head
      && base[base.length - 1 - tail] === current[current.length - 1 - tail]
    ) {
      tail += 1;
    }
    const baseMiddle = base.slice(head, base.length - tail);
    const currentMiddle = current.slice(head, current.length - tail);
    const rows = baseMiddle.length;
    const cols = currentMiddle.length;
    if (!rows && !cols) return marks;
    if (!rows) {
      for (let index = 0; index < cols; index += 1) marks[head + index] = "add";
      return marks;
    }
    // 差异区过大：逐行比对已经没有意义，整段按“已修改”提示
    if (!cols || rows > DIFF_LIMIT || cols > DIFF_LIMIT) {
      const limit = Math.min(current.length, head + cols);
      for (let index = head; index < limit; index += 1) marks[index] = "mod";
      // 纯删除时差异区为空，标记落到删除位置之后的第一行
      if (!cols) marks[Math.min(head, current.length - 1)] = "mod";
      return marks;
    }

    const dp = Array.from({ length: rows + 1 }, () => new Uint32Array(cols + 1));
    for (let i = rows - 1; i >= 0; i -= 1) {
      for (let j = cols - 1; j >= 0; j -= 1) {
        dp[i][j] = baseMiddle[i] === currentMiddle[j]
          ? dp[i + 1][j + 1] + 1
          : Math.max(dp[i + 1][j], dp[i][j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    let removed = false;
    while (i < rows && j < cols) {
      if (baseMiddle[i] === currentMiddle[j]) { i += 1; j += 1; removed = false; continue; }
      if (dp[i + 1][j] >= dp[i][j + 1]) { i += 1; removed = true; continue; }
      marks[head + j] = removed ? "mod" : "add";
      j += 1;
      removed = false;
    }
    while (j < cols) {
      marks[head + j] = removed ? "mod" : "add";
      j += 1;
      removed = false;
    }
    if (removed) marks[Math.min(head + cols, current.length - 1)] = "mod";
    return marks;
  }

