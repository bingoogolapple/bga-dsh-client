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
    // 用 DOM API 构建，不要用 innerHTML 拼模板字符串：s.ip 来自网络侧
    // （配对会话记录的来源 IP），拼进 HTML 会造成 XSS。即使当前 Rust 侧只写入
    // 规范化的 IP 字符串，也不该让渲染层成为唯一防线（纵深防御 + CSP 之外的第二道）。
    const deviceList = $("pair-devices");
    deviceList.replaceChildren();
    const sessions = info.sessions || [];
    if (!sessions.length) {
      const li = document.createElement("li");
      li.className = "empty";
      li.textContent = t("lan.devices.empty");
      deviceList.appendChild(li);
      return;
    }
    for (const s of sessions) {
      const loop = s.ip === "127.0.0.1" || s.ip === "::1";
      const ip = loop ? `${s.ip}${t("lan.session.tunnel_suffix")}` : s.ip;
      const li = document.createElement("li");
      li.title = loop ? t("lan.session.tunnel_title") : t("lan.session.ip_title");
      li.textContent = t("lan.session.title", { 0: ip, 1: s.minutes_left });
      deviceList.appendChild(li);
    }
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
    if (name === "versions") {
      // 快路径秒回本地缓存，慢路径后台拉 npm registry（事件驱动，不阻塞 UI）
      dshRefreshVersions();
      dshSyncRemote();
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
  // 服务拉起方式已完全自动（普通版 npx / 内置版内置 Node.js），无需配置。
  let cfg = { stop_service_on_quit: false };
  try {
    cfg = await invoke("get_settings");
  } catch (e) {
    /* keep defaults */
  }

  // 是否内置 Node.js 运行时：仅用于版本区选择展示内置 runtime 版本还是系统 PATH 版本
  //（启动方式本身由后端自动判定，前端无需配置）。
  let hasRuntime = false;
  try {
    hasRuntime = await invoke("has_bundled_runtime");
  } catch (e) {
    /* 命令不可用时按普通版处理 */
  }
  // 根据内置版/普通版展示下载说明
  const noteEl = $("versions-note");
  if (noteEl) {
    noteEl.textContent = t("versions.download_note");
  }

  // 变更即保存：无需手动点保存按钮
  function persist() {
    const stop = $("stop-service-on-quit").checked;
    invoke("save_settings", { stopServiceOnQuit: stop }).catch((e) => toast(String(e)));
  }

  $("stop-service-on-quit").checked = cfg.stop_service_on_quit !== false;
  $("stop-service-on-quit").onchange = persist;

  // ---------- 隐私：匿名使用统计（默认关闭，opt-in） ----------
  // 与"退出行为"分开保存：遥测走独立命令 set_telemetry_enabled，
  // 因为它除了落盘 settings.json 还要在 Rust 侧即时切换运行期开关。
  const telemetryBox = $("telemetry-enabled");
  if (telemetryBox) {
    telemetryBox.checked = cfg.telemetry_enabled === true;
    telemetryBox.onchange = () => {
      invoke("set_telemetry_enabled", { enabled: telemetryBox.checked }).catch((e) => {
        // 保存失败时把开关拨回原状态，避免 UI 显示与实际行为不一致
        telemetryBox.checked = !telemetryBox.checked;
        toast(String(e));
      });
    };
  }

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
  // 展示流程：打开即渲染后端返回的缓存结果（秒回，不阻塞窗口），后端后台线程
  // 重新完整探测后 emit `version-refreshed`，此处刷新为新值。
  function renderVersions(v) {
    if (!v) return;
    const use = hasRuntime ? v.runtime : v.system;
    $("v-node").textContent = use.node;
    $("v-pnpm").textContent = use.pnpm;
    $("v-dsh").textContent =
      v.running ?? (v.service_up ? t("side.version_unknown") : use.dsh);
    $("v-src").textContent = hasRuntime ? t("side.src_builtin") : t("side.src_system");
  }
  (async () => {
    try {
      renderVersions(await invoke("get_version_info"));
    } catch (e) {
      /* 保持「–」占位 */
    }
  })();
  // 后端后台线程完整探测完成后推送新值（首次打开后 0~8s 内刷新）。
  listen("version-refreshed", (e) => renderVersions(e.payload));

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

  // ---------- dsh 版本管理面板 ----------
  const dshVersionsTbody = $("versions-tbody");
  const dshActiveVer = $("dsh-active-ver");
  const dshClearVer = $("dsh-clear-ver");
  const dshRefreshBtn = $("dsh-refresh-versions");
  const dshLoading = $("versions-loading");

  // npm 下载源选择（radio buttons）
  const registryRadios = document.querySelectorAll('input[name="npm-registry"]');
  // 初始化：从后端读取当前选择并同步 radio 状态
  (async function loadRegistry() {
    try {
      const current = await invoke("dsh_get_registry");
      if (current) {
        const radio = document.querySelector(`input[name="npm-registry"][value="${current}"]`);
        if (radio) radio.checked = true;
      }
    } catch (e) { /* keep default checked */ }
  })();
  // radio 切换时保存到后端
  registryRadios.forEach((radio) => {
    radio.addEventListener("change", () => {
      if (radio.checked) {
        invoke("dsh_set_registry", { registry: radio.value }).catch((e) => toast(String(e)));
      }
    });
  });

  // 渲染版本列表；当前使用版本从列表条目的 active 标记推导（后端已合并 settings.dsh_version）
  function renderDshVersions(versions) {
    const list = versions || [];
    const activeEntry = list.find((v) => v.active);
    if (activeEntry) {
      dshActiveVer.textContent = activeEntry.version;
      dshClearVer.classList.remove("hidden");
    } else {
      dshActiveVer.textContent = hasRuntime
        ? t("versions.default_bundled")
        : t("versions.default_plain");
      dshClearVer.classList.add("hidden");
    }
    // 恢复默认后的启动方式说明（bundled / npx）已移入「恢复默认」二次确认弹窗
    // 清空表格
    dshVersionsTbody.innerHTML = "";
    if (!list.length) {
      const tr = document.createElement("tr");
      const td = document.createElement("td");
      td.colSpan = 3;
      td.textContent = t("versions.loading");
      td.style.textAlign = "center";
      td.style.opacity = "0.5";
      tr.appendChild(td);
      dshVersionsTbody.appendChild(tr);
      return;
    }
    for (const v of list) {
      const tr = document.createElement("tr");
      if (v.active) tr.classList.add("versions-row-active");
      // 版本列
      const tdVer = document.createElement("td");
      tdVer.textContent = v.version;
      tr.appendChild(tdVer);
      // 状态列
      const tdStatus = document.createElement("td");
      const parts = [];
      if (v.builtin) parts.push(t("versions.status.builtin"));
      if (v.local) {
        parts.push(v.active ? t("versions.status.active") : t("versions.status.downloaded"));
      } else if (!v.builtin) {
        parts.push(t("versions.status.available"));
      }
      tdStatus.textContent = parts.join(" · ");
      tr.appendChild(tdStatus);
      // 操作列
      const tdAction = document.createElement("td");
      if (v.active) {
        // 当前使用中：无操作按钮
      } else if (v.local) {
        // 已下载但非使用中
        const useBtn = document.createElement("button");
        useBtn.className = "ghost";
        useBtn.textContent = t("versions.action.use");
        useBtn.onclick = () => dshSetActive(v.version);
        tdAction.appendChild(useBtn);
        if (!v.builtin) {
          const delBtn = document.createElement("button");
          delBtn.className = "ghost action-danger";
          delBtn.textContent = t("versions.action.delete");
          if (deletingVersions.has(v.version)) {
            delBtn.disabled = true;
            delBtn.textContent = t("versions.action.deleting");
          }
          delBtn.onclick = () => dshDeleteVersion(v.version);
          tdAction.appendChild(delBtn);
        }
      } else {
        // 可下载
        const dlBtn = document.createElement("button");
        dlBtn.className = "ghost";
        dlBtn.textContent = t("versions.action.download");
        if (downloadingVersions.has(v.version)) {
          dlBtn.disabled = true;
          dlBtn.textContent = t("versions.action.downloading");
        }
        dlBtn.onclick = () => dshDownloadVersion(v.version);
        tdAction.appendChild(dlBtn);
      }
      tr.appendChild(tdAction);
      dshVersionsTbody.appendChild(tr);
    }
  }

  // 快路径：本地已下载 + 内置 + 缓存远程列表（秒回，无网络，不卡 UI）
  async function dshRefreshVersions() {
    dshLoading.classList.remove("hidden");
    try {
      const result = await invoke("dsh_list_versions");
      renderDshVersions(result);
    } catch (e) {
      toast(String(e));
    } finally {
      dshLoading.classList.add("hidden");
    }
  }

  // 慢路径：缓存过期（>1小时）时后台拉取 npm registry，未过期则跳过。
  // 完成经 dsh-versions-refreshed 事件更新。
  function dshSyncRemote() {
    dshRefreshBtn.disabled = true;
    invoke("dsh_maybe_refresh_remote_versions")
      .catch(() => {})
      .finally(() => {
        dshRefreshBtn.disabled = false;
      });
  }

  // 进行中的下载集合：防止重复点击造成并发竞态（后端也有防重入，前端禁用更友好）
  const downloadingVersions = new Set();

  async function dshDownloadVersion(version) {
    if (downloadingVersions.has(version)) return;
    downloadingVersions.add(version);
    toast(t("versions.downloading", { 0: version }));
    try {
      await invoke("dsh_download_version", { version });
    } catch (e) {
      downloadingVersions.delete(version);
      toast(String(e));
      dshRefreshVersions();
    }
  }

  async function dshSetActive(version) {
    try {
      const ok = await confirmDialog(
        t("versions.set_active_confirm", { 0: version }),
        {
          title: t("versions.set_active_title"),
          okText: t("versions.restart_switch"),
          cancelText: t("versions.cancel_switch"),
        },
      );
      if (!ok) return;
      await invoke("dsh_set_active_version", { version });
      await dshRefreshVersions();
      // 立即把左下角 dsh 版本同步为刚切换到的版本（后台版本探测可能稍慢/失败，
      // 避免重启后左下角仍显示切换前的旧版本）。
      const activeVersion = $("dsh-active-ver").textContent.trim();
      if (activeVersion) $("v-dsh").textContent = activeVersion;
      toast(t("versions.restarting"));
      await invoke("service_restart");
      // 轮询等待服务真正启动后再探测版本（最多等 15 秒）
      for (let i = 0; i < 30; i++) {
        await new Promise((r) => setTimeout(r, 500));
        try {
          const st = await invoke("query_status");
          if (st && st.state === "running") break;
        } catch (_) { /* 忽略，服务尚未就绪 */ }
      }
      await invoke("force_refresh_version_info");
    } catch (e) {
      toast(String(e));
    }
  }

  // 进行中的删除集合：防止 confirm 框连点/重复触发
  const deletingVersions = new Set();

  async function dshDeleteVersion(version) {
    if (deletingVersions.has(version)) return;
    const ok = await confirmDialog(t("versions.confirm_delete", { 0: version }), {
      title: t("versions.confirm_delete_title"),
      okText: t("versions.action.delete"),
      cancelText: t("common.cancel"),
    });
    if (!ok) return;
    deletingVersions.add(version);
    toast(t("versions.deleting"));
    // 删除在后台线程执行（node_modules 文件多时耗时较长），结果经
    // dsh-versions-refreshed（action=delete）事件返回，期间按钮显示「删除中…」
    try {
      await invoke("dsh_delete_version", { version });
    } catch (e) {
      // 同步快速校验失败（使用中 / 未下载）：直接提示
      deletingVersions.delete(version);
      toast(String(e));
    }
  }

  dshClearVer.onclick = async () => {
    const ok = await confirmDialog(t("versions.cleared_confirm"), {
      title: t("versions.set_active_title"),
      // 二次确认弹窗中说明恢复默认后的启动方式（内置版 bundled / 普通版 npx）
      detail: hasRuntime ? t("versions.clear_hint_bundled") : t("versions.clear_hint_plain"),
      okText: t("versions.restart_switch"),
      cancelText: t("versions.cancel_switch"),
    });
    if (!ok) return;
    try {
      await invoke("dsh_set_active_version", { version: null });
      await dshRefreshVersions();
      toast(t("versions.restarting"));
      await invoke("service_restart");
      for (let i = 0; i < 30; i++) {
        await new Promise((r) => setTimeout(r, 500));
        try {
          const st = await invoke("query_status");
          if (st && st.state === "running") break;
        } catch (_) {}
      }
      await invoke("force_refresh_version_info");
    } catch (e) {
      toast(String(e));
    }
  };

  // 刷新按钮：先同步渲染本地（秒回），再触发后台远程刷新
  dshRefreshBtn.onclick = () => {
    dshRefreshVersions();
    dshSyncRemote();
  };

  // 远程刷新 / 删除完成事件（后台线程执行后广播，不冻结 UI）
  listen("dsh-versions-refreshed", (e) => {
    const payload = e.payload || {};
    if (payload.ok) {
      if (payload.action === "delete") deletingVersions.clear();
      renderDshVersions(payload.list);
      if (payload.action === "delete") toast(t("versions.deleted"));
    } else {
      // 删除失败也需解除按钮禁用状态，否则该版本按钮将一直卡在「删除中…」
      deletingVersions.clear();
      renderDshVersions(payload.list);
      toast(String(payload.error || ""));
    }
    dshRefreshBtn.disabled = false;
  });

  // 下载进度事件
  listen("dsh-download-progress", (e) => {
    const { version, stage, message } = e.payload || {};
    if (stage === "installing") {
      // npm 开始安装：按钮已显示「下载中…」，这里给出明确提示
      toast(t("versions.downloading", { 0: version }));
      dshRefreshVersions();
    } else if (stage === "done") {
      downloadingVersions.delete(version);
      toast(t("versions.download_done", { 0: version }));
      dshRefreshVersions();
    } else if (stage === "error") {
      downloadingVersions.delete(version);
      toast(t("versions.download_error", { 0: version, 1: message }));
      dshRefreshVersions();
    }
  });

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
      renderVersions(v);
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
    // 版本管理面板语言切换时重新渲染
    if (panelEls.get("versions") && panelEls.get("versions").classList.contains("active")) {
      dshRefreshVersions();
    }
    // 更新版本管理说明文案
    const noteEl2 = $("versions-note");
    if (noteEl2) {
      noteEl2.textContent = t("versions.download_note");
    }
  });
})();