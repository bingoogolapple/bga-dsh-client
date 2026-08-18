/* Settings window: sidebar menu + panels (general / service / lan). */

(async function () {
  // ---------- 侧边菜单 ----------
  const navItems = [...document.querySelectorAll(".nav-item")];
  const panelEls = new Map(navItems.map((n) => [n.dataset.panel, $(`panel-${n.dataset.panel}`)]));
  let lanTimer = null;

  async function refreshLan() {
    try {
      renderLan(await invoke("get_pairing_info"));
    } catch (e) {
      $("pair-status").textContent = String(e);
      $("pair-status").dataset.state = "error";
    }
  }

  // 代理按钮可用性：
  // - 启动代理：服务在线 && 代理未运行 → 运行中禁用
  // - 重启代理：服务在线 && 代理运行中 → 未运行禁用
  // - 停止代理：代理运行中 → 未运行禁用（服务挂掉时仍可停掉空转代理）
  // - 复制链接 / 复制二维码 / 重新生成配对码：代理未运行 → 直接禁用
  let agentServiceUp = false;
  let agentRunning = false;

  function applyAgentDisabled() {
    $("btn-pair-start").disabled = !agentServiceUp || agentRunning;
    $("btn-pair-restart").disabled = !agentServiceUp || !agentRunning;
    $("btn-pair-stop").disabled = !agentRunning;
    $("btn-pair-copy-url").disabled = !agentRunning;
    $("btn-pair-copy-qr").disabled = !agentRunning;
    $("btn-pair-regen").disabled = !agentRunning;
  }

  function renderLan(info) {
    const on = info.running;
    // 代理停止：二维码 / 配对码 / 完整访问地址一并清空（旧 URL 可能已失效）
    $("pair-qr").innerHTML = on ? info.qr || "" : "";
    $("pair-code").textContent = on ? info.code || "------" : "";
    $("pair-addr").textContent = on
      ? `http://${info.ip}:${info.port}/?pair=${info.code}`
      : "";
    const pill = $("pair-status");
    if (!info.running) {
      pill.textContent = t("lan.status.stopped");
      pill.dataset.state = "none";
    } else {
      pill.textContent = t(info.service_up ? "lan.status.running_up" : "lan.status.running_down");
      pill.dataset.state = info.service_up ? "running" : "none";
    }
    agentRunning = info.running;
    applyAgentDisabled();
    // 已配对设备 = 浏览器会话（扫码配对，身份跟着 Cookie 走，局域网/内网穿透通用）。
    // 列表展示配对时的来源 IP 与剩余时间；经 localhost.run 等隧道访问的会话
    // 来源 IP 统一为 127.0.0.1（隧道把外网折叠成本机回环，IP 不具区分度）。
    const list = (info.sessions || []).map((s) => {
      const loop = s.ip === "127.0.0.1" || s.ip === "::1";
      const ip = loop ? `${s.ip}${t("lan.session.tunnel_suffix")}` : s.ip;
      const title = loop ? t("lan.session.tunnel_title") : t("lan.session.ip_title");
      return `<li title="${title}">${t("lan.session.title", { 0: ip, 1: s.minutes_left })}</li>`;
    });
    const deviceList = $("pair-devices");
    deviceList.innerHTML = list.length
      ? list.join("")
      : `<li class="empty">${t("lan.devices.empty")}</li>`;
  }

  function switchPanel(name) {
    navItems.forEach((n) => n.classList.toggle("active", n.dataset.panel === name));
    panelEls.forEach((el, key) => el.classList.toggle("active", key === name));
    // 日志历史懒加载：切到对应面板才拉首屏（打开窗口/切别的面板不读日志）
    if (name === "service") serviceLog.loadOnce("read_service_log");
    if (name === "lan") {
      pairLog.loadOnce("read_pairing_log");
      // 局域网面板激活时才轮询，避免后台空转
      refreshLan();
      if (!lanTimer) lanTimer = setInterval(refreshLan, 3000);
    } else if (lanTimer) {
      clearInterval(lanTimer);
      lanTimer = null;
    }
  }

  navItems.forEach((n) => (n.onclick = () => switchPanel(n.dataset.panel)));

  // 托盘服务操作结束自动打开设置页时，后端通过 eval 调用此入口切换面板
  //（仅窗口已存在时使用；新建窗口走 URL ?panel= 参数）。
  window.__openPanel = switchPanel;

  // ---------- 版本 ----------
  try {
    $("app-version").textContent = "v" + (await invoke("get_app_version"));
  } catch (e) {
    /* keep "-" */
  }

  // ---------- 应用更新检测 ----------
  let updateInfo = null;

  // 状态行渲染：idle（未检查）/ checking（检查中）/ error（失败）/
  // has_update（发现新版本，可前往下载/忽略）/ dismissed（已忽略当前版本）/ latest（已是最新）
  function renderUpdateStatus(info) {
    updateInfo = info || updateInfo || {};
    const el = $("update-status");
    const btn = $("btn-check-update");
    if (!el) return;
    const i = updateInfo;
    el.classList.remove("has-update", "latest", "error", "checking");
    el.innerHTML = "";
    if (i.status === "checking") {
      btn.disabled = true;
      el.hidden = false;
      el.classList.add("checking");
      el.textContent = t("update.checking");
      return;
    }
    btn.disabled = false;
    if (i.status === "error") {
      el.hidden = false;
      el.classList.add("error");
      el.textContent = t("update.failed");
      return;
    }
    if (i.has_update && !i.dismissed) {
      el.hidden = false;
      el.classList.add("has-update");
      const t0 = document.createElement("span");
      t0.textContent = t("update.found", { 0: i.latest });
      const a = document.createElement("a");
      a.textContent = t("update.download");
      a.onclick = (e) => {
        e.preventDefault();
        invoke("open_download_page");
      };
      const ig = document.createElement("a");
      ig.className = "update-ignore";
      ig.textContent = t("update.ignore");
      ig.onclick = (e) => {
        e.preventDefault();
        invoke("dismiss_update");
      };
      el.append(t0, a, ig);
      return;
    }
    if (i.has_update && i.dismissed) {
      el.hidden = false;
      el.textContent = t("update.ignored", { 0: i.latest });
      const a = document.createElement("a");
      a.textContent = t("update.download");
      a.onclick = (e) => {
        e.preventDefault();
        invoke("open_download_page");
      };
      el.append(a);
      return;
    }
    if (i.latest) {
      el.hidden = false;
      el.classList.add("latest");
      el.textContent = t("update.latest", { 0: i.latest });
      return;
    }
    el.hidden = true;
  }

  try {
    renderUpdateStatus(await invoke("get_update_info"));
  } catch (e) {
    /* IPC 未就绪：状态行保持隐藏 */
  }

  listen("update-available", (e) => renderUpdateStatus(e.payload));

  $("btn-check-update").onclick = () => {
    renderUpdateStatus({ ...(updateInfo || {}), status: "checking" });
    invoke("check_for_update").catch(() =>
      renderUpdateStatus({ ...(updateInfo || {}), status: "error" })
    );
  };

  // ---------- 常规设置面板 ----------
  const radios = document.querySelectorAll('input[name="launch_method"]');
  const pnpmRow = $("pnpm-row");
  const launchDirInput = $("launch-dir-input");
  const btnPick = $("btn-pick");
  const methodSection = $("method-section");
  const methodNoteBuiltin = $("method-note-builtin");

  let cfg = { launch_method: "npx", launch_dir: "", stop_service_on_quit: false };
  try {
    cfg = await invoke("get_settings");
  } catch (e) {
    /* keep defaults */
  }

  // 内置版（应用自带 runtime）：强制使用内置 Node.js，隐藏整个「服务拉起方式」模块；
  // 非内置版暴露方式一/二/三（不再有方式四卡片）。
  // 注：方式四是执行语义（rust 侧强制内置/回退），前端已无对应卡片。
  let hasRuntime = false;
  try {
    hasRuntime = await invoke("has_bundled_runtime");
    if (hasRuntime) {
      methodSection.classList.add("hidden");
      methodNoteBuiltin.classList.remove("hidden");
    }
    // 注：项目未发布过，无历史配置文件——非内置版不会读到 builtin 值，
    // 不再做 builtin→npx 的回落迁移（已删除）。
  } catch (e) {
    /* 命令不可用时按内置版处理，卡片保持可见 */
  }

  function currentMethod() {
    return [...radios].find((r) => r.checked).value;
  }

  function sync() {
    pnpmRow.classList.toggle("hidden", currentMethod() !== "pnpm");
  }

  // 变更即保存：无需手动点保存按钮
  // 内置版不暴露方式配置，固定写回 builtin（rust 侧启动时亦强制内置）
  function persist() {
    const stop = $("stop-service-on-quit").checked;
    const payload = hasRuntime
      ? { launchMethod: "builtin", launchDir: "", stopServiceOnQuit: stop }
      : {
          launchMethod: currentMethod(),
          launchDir: launchDirInput.value.trim(),
          stopServiceOnQuit: stop,
        };
    invoke("save_settings", payload).catch((e) => toast(String(e)));
  }

  radios.forEach((r) => {
    r.checked = r.value === cfg.launch_method;
    r.onchange = () => {
      sync();
      persist();
    };
  });
  launchDirInput.value = cfg.launch_dir || "";
  $("stop-service-on-quit").checked = cfg.stop_service_on_quit !== false;
  $("stop-service-on-quit").onchange = persist;
  sync();

  btnPick.onclick = async () => {
    const d = await invoke("pick_dir");
    if (d) {
      launchDirInput.value = d;
      persist();
    }
  };

  // ---------- 服务控制面板 ----------
  let lastStat = null;
  function applyStat(info) {
    lastStat = info;
    $("s-pill").textContent = stateLabel(info.state);
    $("s-pill").dataset.state = info.state;
    // 与托盘一致：运行中禁「启动」；停止/重启仅对本应用启动的服务开放
    // （外部服务三个都不许点）；启动中全禁。
    const running = info.state === "running";
    const starting = info.state === "starting";
    const manageable = running && !!info.mine && !starting;
    $("s-start").disabled = running || starting;
    $("s-restart").disabled = !manageable;
    $("s-stop").disabled = !manageable;
    // 代理依赖上游 127.0.0.1:3080：服务在线才能启动/重启代理；
    // 停止代理始终可用（随时可关掉空转的代理）。
    const serviceUp = running && !starting;
    agentServiceUp = serviceUp;
    applyAgentDisabled();
  }

  listen("service-status", (e) => applyStat(e.payload));
  $("s-start").onclick = () => invoke("service_start");
  $("s-restart").onclick = () => invoke("service_restart");
  $("s-stop").onclick = () => invoke("service_stop");
  $("btn-main").onclick = () => invoke("show_main_window");

  // ---------- 日志区工厂（服务日志 / 代理日志共用） ----------
  function makeLogArea(view, emptyId, emptyKey) {
    let loaded = false;
    function ensureEmpty() {
      if (!document.getElementById(emptyId)) {
        const p = document.createElement("div");
        p.id = emptyId;
        p.className = "log-empty";
        p.textContent = t(emptyKey);
        view.appendChild(p);
      }
    }
    // 语言切换时刷新空态文案（日志内容本身不动）。
    function updateEmpty() {
      if (loaded && !view.querySelector(".log-line")) {
        view.innerHTML = "";
        ensureEmpty();
      }
    }
    // 追加一行：内容原样渲染。时间戳由后端写入时生成（now_ts，落盘+事件一致），
    // 前端不再补当前时间——保证刷新重读后同一行的时间戳稳定不变。
    function append(line) {
      if (!line) return;
      const empty = document.getElementById(emptyId);
      if (empty) empty.remove();
      const div = document.createElement("div");
      div.className = "log-line";
      div.textContent = String(line);
      view.appendChild(div);
      view.scrollTop = view.scrollHeight; // 始终跟随最新
    }
    async function reload(cmd) {
      view.innerHTML = "";
      ensureEmpty();
      try {
        const lines = await invoke(cmd);
        if (!Array.isArray(lines) || !lines.length) return;
        // 批量构建后一次性插入，避免 200 行逐个 append 重排
        const frag = document.createDocumentFragment();
        for (const l of lines) {
          if (!l) continue;
          const d = document.createElement("div");
          d.className = "log-line";
          d.textContent = String(l);
          frag.appendChild(d);
        }
        const empty = document.getElementById(emptyId);
        if (empty) empty.remove();
        view.appendChild(frag);
        view.scrollTop = view.scrollHeight;
      } catch (e) {
        /* 日志文件不存在等：保留空态 */
      }
    }
    // 懒加载：首次进入对应面板才拉历史；刷新按钮强制重读
    function loadOnce(cmd) {
      if (!loaded) {
        loaded = true;
        reload(cmd);
      }
    }
    function forceReload(cmd) {
      loaded = true;
      reload(cmd);
    }
    return { append, reload, loadOnce, forceReload, updateEmpty };
  }

  const serviceLog = makeLogArea($("s-log"), "s-log-empty", "svc.log.empty");
  // 历史在切到「服务控制」面板时懒加载（loadOnce），避免打开设置窗口即全量读大日志
  $("s-log-refresh").onclick = () => serviceLog.forceReload("read_service_log");
  listen("service-log", (e) => serviceLog.append(String((e.payload && e.payload.line) || "")));

  const pairLog = makeLogArea($("p-log"), "p-log-empty", "lan.log.empty");
  $("p-log-refresh").onclick = () => pairLog.forceReload("read_pairing_log");
  listen("pairing-log", (e) => pairLog.append(String((e.payload && e.payload.line) || "")));

  // ---------- 侧边栏工具版本（node/pnpm/dsh） ----------
  // dsh：服务在线时优先展示运行中服务自报的真实版本（npx 拉起的也能拿到）；
  // 服务在线但查不到版本（旧版服务 host.describe 返回占位符/探测失败）时展示
  // 「版本未知」，避免误导成「未安装」；否则内置包展示内置 runtime 版本、
  // 非内置包展示系统 PATH 生效版本。
  (async () => {
    try {
      const v = await invoke("get_version_info");
      const use = hasRuntime ? v.runtime : v.system;
      $("v-node").textContent = use.node;
      $("v-pnpm").textContent = use.pnpm;
      $("v-dsh").textContent =
        v.running ?? (v.service_up ? t("side.version_unknown") : use.dsh);
      $("v-src").textContent = hasRuntime ? t("side.src_builtin") : t("side.src_system");
    } catch (e) {
      /* 保持「–」占位 */
    }
  })();

  // ---------- 局域网面板动作 ----------
  async function runLan(fn, okMsg) {
    try {
      await fn();
      if (okMsg) toast(okMsg);
      await refreshLan();
    } catch (e) {
      toast(String(e));
    }
  }

  // 仅代理运行时可用的操作：点击时二次校验（禁用态与 3s 轮询之间可能有几秒竞态窗口，
  // 在此硬拦截，避免触发后端「代理未启动」的报错 toast）。
  function lanOnly(fn, okMsg) {
    if (!agentRunning) return;
    runLan(fn, okMsg);
  }

  $("btn-pair-copy-url").onclick = () =>
    lanOnly(() => invoke("copy_pairing_url"), t("toast.url_copied"));
  $("btn-pair-copy-qr").onclick = () =>
    lanOnly(() => invoke("copy_qr_image"), t("toast.qr_copied"));
  $("btn-pair-regen").onclick = () =>
    lanOnly(() => invoke("pairing_regen"), t("toast.code_regen"));
  $("btn-pair-stop").onclick = () => runLan(() => invoke("pairing_stop"), t("toast.proxy_stopped"));
  $("btn-pair-start").onclick = () => runLan(() => invoke("pairing_start"), t("toast.proxy_started"));
  $("btn-pair-restart").onclick = () =>
    runLan(() => invoke("pairing_restart"), t("toast.proxy_restarted"));

  // ---------- 打赏支持作者面板 ----------
  $("btn-subscribe-opencode").onclick = () => {
    invoke("open_opencode_ref").catch(() => {});
    toast(t("toast.opencode_open"));
  };

  // ---------- 启动 ----------
  try {
    applyStat(await invoke("query_status"));
  } catch (e) {
    /* not ready yet */
  }

  // 托盘服务操作结束自动打开设置页（新建窗口）时，URL 携带 ?panel= 参数，启动后定位到对应面板。
  const panelFromUrl = new URLSearchParams(location.search).get("panel");
  if (panelFromUrl && panelEls.has(panelFromUrl)) switchPanel(panelFromUrl);

  // ---------- 语言切换重渲染 ----------
  // i18n.js 已把静态 [data-i18n] 元素替换为当前语言；这里重渲染 JS 生成的动态文案：
  // 版本来源、更新状态、服务/局域网状态与空日志占位。
  window.addEventListener("dsh:locale", async () => {
    try {
      const v = await invoke("get_version_info");
      const use = hasRuntime ? v.runtime : v.system;
      $("v-node").textContent = use.node;
      $("v-pnpm").textContent = use.pnpm;
      $("v-dsh").textContent =
        v.running ?? (v.service_up ? t("side.version_unknown") : use.dsh);
      $("v-src").textContent = hasRuntime ? t("side.src_builtin") : t("side.src_system");
    } catch (e) {
      /* keep placeholders */
    }
    if (updateInfo) renderUpdateStatus(updateInfo);
    if (lastStat) applyStat(lastStat);
    if (lanTimer) {
      try {
        renderLan(await invoke("get_pairing_info"));
      } catch (e) {
        /* keep last */
      }
    }
    serviceLog.updateEmpty();
    pairLog.updateEmpty();
  });
})();