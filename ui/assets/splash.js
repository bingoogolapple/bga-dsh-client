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
  let errorLogLoaded = false;

  // 服务每次启动都会换一个新的启动令牌，所以地址要每轮都问一次；是否真的
  // 重新加载由 loadFrame 判断（地址没变就不动，令牌没抓到时不降级）。
  let frameSeq = 0;

  async function loadFrame() {
    const seq = ++frameSeq;
    let url = DSH_URL;
    try {
      url = await invoke("dsh_launch_url");
    } catch (e) {
      /* Tauri IPC 尚未就绪：退回常量地址。 */
    }
    // apply 可能已被更新的调用重新进入（poll 与事件并发），丢弃过期结果。
    if (seq !== frameSeq) return;
    const src = els.frame.getAttribute("src") || "";
    if (src === url) return;
    // 新地址不带令牌，说明本次启动的令牌还没抓到（或这就是旧版 dsh）。
    // 此时若页面已经用令牌加载过，就别把它降级成裸地址——保持现状等下一轮，
    // 免得把好好的会话换成一次 401。
    if (url.indexOf("?token=") === -1 && src.startsWith(DSH_URL)) return;
    els.frame.setAttribute("src", url);
  }

  function apply(info) {
    const running = info.state === "running";
    els.pill.textContent = stateLabel(info.state);
    els.pill.dataset.state = info.state;

    if (running) {
      // 每轮都问一次地址：服务重启后令牌会变，页面必须跟着换，否则旧页面
      // 在 WebSocket 断开后就白屏了。是否真重载由 loadFrame 判断（地址没变就不动）。
      loadFrame();
      errorLogLoaded = false;
      els.frameWrap.classList.remove("hidden");
      els.splash.classList.add("hidden");
    } else {
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
