/* Main window: splash + in-app iframe host for the DSH Web GUI. */

(async function () {
  const els = {
    pill: $("status-pill"),
    splash: $("splash"),
    frameWrap: $("frame-wrap"),
    frame: $("frame"),
    splashStatus: $("splash-status"),
    spinner: $("spinner"),
    logView: $("log-view"),
    btnStart: $("btn-start"),
    btnRetry: $("btn-retry"),
    btnSettings: $("btn-settings"),
    btnOpenBrowser: $("btn-open-browser"),
    btnRefresh: $("btn-refresh"),
  };

  const logs = [];
  let errorLogLoaded = false;

  // 服务每次启动都会换一个新的启动令牌，所以地址要每轮都问一次；是否真的
  // 重新加载由 loadFrame 判断（地址没变就不动，令牌没抓到时不降级）。
  let frameGeneration = 0;

  async function loadFrame(force = false) {
    const generation = frameGeneration;
    let url = DSH_URL;
    try {
      url = await invoke("dsh_launch_url");
    } catch (e) {
      /* Tauri IPC 尚未就绪：退回常量地址。 */
    }
    // apply 可能已被更新的调用重新进入（poll 与事件并发），丢弃过期结果。
    if (generation !== frameGeneration) return;
    const src = els.frame.getAttribute("src") || "";
    if (!force && src === url) return;
    // 新地址不带令牌，说明本次启动的令牌还没抓到（或这就是旧版 dsh）。
    // 此时若页面已经用令牌加载过，就别把它降级成裸地址——保持现状等下一轮，
    // 免得把好好的会话换成一次 401。
    if (!force && url.indexOf("?token=") === -1 && src.startsWith(DSH_URL)) return;
    // Legacy dsh has no token, so the URL is identical after every restart.
    // A fragment is not sent to the server, but makes WebView perform a real
    // navigation instead of reusing the old iframe document/WebSocket.
    const navigationUrl =
      force && url.indexOf("?token=") === -1 ? `${url}#dsh-reload=${Date.now()}` : url;
    els.frame.setAttribute("src", navigationUrl);
  }

  let wasRunning = false;
  let lastStatusRevision = -1;
  let reloadOnRunning = false;
  let restartInProgress = false;
  let frameRetryTimers = [];

  function retryFrameLoads() {
    // A 401 response also fires iframe "load", so it cannot be treated as a
    // successful authenticated page. Re-query the launch URL after startup;
    // loadFrame(false) navigates only when a newly captured token changes it.
    for (const delay of [1000, 3000]) {
      frameRetryTimers.push(
        setTimeout(() => {
          if (wasRunning) loadFrame(false);
        }, delay),
      );
    }
  }

  listen("service-restarting", () => {
    frameGeneration += 1;
    reloadOnRunning = true;
    restartInProgress = true;
    for (const timer of frameRetryTimers) clearTimeout(timer);
    frameRetryTimers = [];
  });

  function apply(info) {
    // The event stream and query_status may race. ServiceInfo.revision is the
    // monotonic snapshot generation; stale events must not overwrite a newer
    // lifecycle state in the splash screen.
    const revision = Number.isFinite(info?.revision) ? info.revision : 0;
    if (revision < lastStatusRevision) return;
    lastStatusRevision = revision;
    const running = info.state === "running";
    els.pill.textContent = stateLabel(info.state);
    els.pill.dataset.state = info.state;

    if (running) {
      // 每轮都问一次地址：服务重启后令牌会变，页面必须跟着换，否则旧页面
      // 在 WebSocket 断开后就白屏了。是否真重载由 loadFrame 判断（地址没变就不动）。
      // Older dsh versions have no launch token, so their URL stays the same
      // across a service restart. Force a reload when returning to running;
      // otherwise the iframe can remain on the disconnected old page and show
      // a blank content area after switching dsh versions.
      const enteringRunning = !wasRunning || reloadOnRunning;
      loadFrame(enteringRunning);
      reloadOnRunning = false;
      restartInProgress = false;
      wasRunning = true;
      if (enteringRunning) retryFrameLoads();
      errorLogLoaded = false;
      els.frameWrap.classList.remove("hidden");
      els.splash.classList.add("hidden");
    } else {
      if (wasRunning) frameGeneration += 1;
      wasRunning = false;
      // During an explicit restart keep the old document visible until the
      // replacement service is ready. Clearing the iframe here causes a
      // visible blank flash, especially with slower legacy dsh versions.
      // 只有启动中的短暂过渡保留旧页面；失败/停止必须结束过渡，
      // 否则用户会看到已断开的 iframe 而拿不到重试入口。
      if (restartInProgress && info.state === "starting") return;
      restartInProgress = false;
      els.frameWrap.classList.add("hidden");
      els.frame.setAttribute("src", "about:blank");
      els.splash.classList.remove("hidden");
      els.spinner.classList.toggle("hidden", info.state !== "starting");
      els.btnStart.classList.toggle("hidden", info.state === "starting");
      els.btnRetry.classList.toggle("hidden", info.state !== "error");
      els.logView.classList.toggle("hidden", info.state !== "error");
      els.splashStatus.textContent = info.detail || stateLabel(info.state);
      // 进入失败态时若实时日志一条都没收到，直接从日志文件兜底拉取末尾，
      // 保证失败原因一定可见（事件可能早于页面挂监听或已错过）。
      if (info.state === "error" && !errorLogLoaded) {
        errorLogLoaded = true;
        invoke("read_service_log", { limit: 300 })
          .then((lines) => {
            if (!lines || !lines.length || els.logView.textContent.trim()) return;
            logs.length = 0;
            logs.push(...lines);
            els.logView.textContent = logs.join("\n");
            els.logView.scrollTop = els.logView.scrollHeight;
          })
          .catch(() => {});
      }
    }
  }

  function appendLog(line) {
    logs.push(line);
    if (logs.length > 300) logs.shift();
    els.logView.textContent = logs.join("\n");
    els.logView.scrollTop = els.logView.scrollHeight;
  }

  listen("service-status", (e) => apply(e.payload));
  listen("service-log", (e) => appendLog(e.payload.line));

  // 语言切换：重新拉取状态，让徽标/详情文案跟随 Harness 语言设置。
  window.addEventListener("dsh:locale", () => poll());

  els.btnStart.onclick = () => invoke("service_start");
  els.btnRetry.onclick = () => invoke("service_start");
  els.btnSettings.onclick = () => invoke("open_settings_window");
  els.btnOpenBrowser.onclick = () => invoke("open_dsh_in_browser");
  els.btnRefresh.onclick = () => location.reload();

  async function poll() {
    try {
      apply(await invoke("query_status"));
    } catch (e) {
      /* Tauri IPC not ready yet; will retry on next tick. */
    }
  }

  await poll();
  setInterval(poll, 4000);
})();
