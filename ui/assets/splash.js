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
    btnRefresh: $("btn-refresh"),
  };

  const logs = [];
  let wasRunning = false;
  let errorLogLoaded = false;
  let lastInfo = null;

  function apply(info) {
    lastInfo = info;
    const running = info.state === "running";
    els.pill.textContent = stateLabel(info.state);
    els.pill.dataset.state = info.state;

    if (running) {
      // 从非运行态进入运行态（含重启服务）时强制重新加载 iframe。
      if (!wasRunning || els.frame.getAttribute("src") !== DSH_URL) {
        els.frame.setAttribute("src", DSH_URL);
      }
      wasRunning = true;
      errorLogLoaded = false;
      els.frameWrap.classList.remove("hidden");
      els.splash.classList.add("hidden");
    } else {
      wasRunning = false;
      els.frameWrap.classList.add("hidden");
      els.frame.setAttribute("src", "about:blank");
      els.splash.classList.remove("hidden");
      els.spinner.classList.toggle("hidden", info.state !== "starting");
      els.btnStart.classList.toggle("hidden", info.state === "starting");
      els.btnRetry.classList.toggle("hidden", info.state !== "error");
      els.logView.classList.toggle("hidden", info.state !== "error");
      els.splashStatus.textContent =
        info.detail || stateLabel(info.state);
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