  /* ==========================================================
     URL 解析
     ========================================================== */
  function parseUrl(raw) {
    try {
      const url = new URL(String(raw || ""));
      let password = "";
      try { password = decodeURIComponent(url.password || ""); } catch (e) { password = url.password || ""; }
      const withoutPassword = new URL(url.toString());
      withoutPassword.password = "";
      return {
        ok: true,
        view: withoutPassword.toString(),
        password,
        protocol: url.protocol.replace(":", ""),
        username: url.username,
        host: url.hostname,
        port: url.port,
        database: url.pathname.replace(/^\//, ""),
        params: Array.from(url.searchParams.entries())
      };
    } catch (error) {
      return { ok: false, view: String(raw || ""), password: "", protocol: "", username: "", host: "", port: "", database: "", params: [] };
    }
  }

  function composeUrl(text, password) {
    try {
      const url = new URL(String(text || ""));
      url.password = password || "";
      return url.toString();
    } catch (error) {
      return String(text || "");
    }
  }

  /* ==========================================================
     接口
     ========================================================== */
  async function api(path, options) {
    const opts = options || {};
    const headers = { "x-agentdb-editor-token": TOKEN };
    if (opts.body) headers["content-type"] = "application/json";
    let response;
    try {
      response = await fetch(path, { method: opts.method || "GET", headers, body: opts.body });
    } catch (error) {
      // 服务可能已经空闲自动退出，给一句能直接照做的提示
      throw new Error("编辑服务未响应（可能已自动关闭），请重新执行 agent-database-cli editor start");
    }
    const text = await response.text();
    let payload = {};
    try { payload = text ? JSON.parse(text) : {}; } catch (e) { payload = { error: text }; }
    if (!response.ok) throw new Error(payload.error || "HTTP " + response.status);
    return payload;
  }

