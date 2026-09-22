  /* ==========================================================
     空闲与保活
     ========================================================== */
  // 与服务端一致：这么久没有请求，服务会自己退出。
  // 页面只在“刚才还动过”时打点，用户停手后就让它自然超时，
  // 避免页面开着却没人操作时进程一直挂着。
  const IDLE_CLOSE_MS = 600000;
  const HEARTBEAT_MS = 60000;
  const ACTIVE_WINDOW_MS = 60000;

  let lastActivity = Date.now();
  let activitySincePing = false;
  let warned = false;
  let serviceLost = false;
  const markActivity = () => {
    lastActivity = Date.now();
    activitySincePing = true;
    warned = false;
  };
  ["pointerdown", "pointermove", "keydown", "wheel", "input", "focus"].forEach((type) =>
    window.addEventListener(type, markActivity, { passive: true })
  );

  setInterval(async () => {
    const idle = Date.now() - lastActivity;
    if (idle >= IDLE_CLOSE_MS - HEARTBEAT_MS && !warned) {
      warned = true;
      toast("长时间无操作，编辑服务即将自动关闭", "err");
      return;
    }
    if (idle >= IDLE_CLOSE_MS || document.hidden) return;
    if (!activitySincePing && idle >= ACTIVE_WINDOW_MS) return;
    activitySincePing = false;
    try {
      await api("/api/health");
      serviceLost = false;
    } catch (error) {
      // 服务已经退出：提醒一次，避免用户继续编辑却保存不进去
      if (!serviceLost) {
        serviceLost = true;
        toast("编辑服务已关闭，重新执行 agent-database-cli editor start 后再操作", "err");
      }
    }
  }, HEARTBEAT_MS);

  /* ==========================================================
     启动
     ========================================================== */
  (async function boot() {
    // 入场动画只在首次进入播放，之后任何重绘都不再播放
    setTimeout(() => document.body.classList.add("booted"), 900);
    try {
      await load();
      if (window.matchMedia("(min-width:1440px)").matches) {
        S.jsonOpen = true;
        document.body.classList.add("json-open");
        $$('[data-act="toggle-json"]').forEach((element) => element.setAttribute("aria-pressed", "true"));
        renderPrint();
      }
    } catch (error) {
      toast(error.message, "err");
    }
  })();
})();
