/* DeepSeekHarness 前端多语言：中文（默认）/ 英文。
 * 语言来源：Rust 侧 i18n::current()（读 Harness 配置 $DSH_HOME/settings.yaml 的 locale.preference），
 * 页面初始化时经 get_locale 命令获取；语言切换时后端广播 locale-changed 事件，
 * 本脚本同步字典并重新渲染 [data-i18n] 元素，随后派发 window "dsh:locale" 事件
 * 让 splash.js / settings.js 重渲染动态文案。
 * 加载顺序：必须先于 splash.js / settings.js（t() 在页内脚本启动时即可用，缺省中文兜底）。 */

(function () {
  const DICT = {
    zh: {
      // 窗口控制 + 顶栏
      "win.close": "关闭窗口",
      "win.close.tooltip": "关闭窗口（收起到托盘）",
      "win.min": "最小化",
      "win.full": "全屏",
      "win.full.tooltip": "全屏 / 退出全屏",
      "top.settings": "客户端设置",
      "top.settings.tooltip": "打开客户端设置",
      "top.refresh": "刷新页面",
      "top.detect": "检测中…",
      "top.show_main": "显示主界面",
      "win.settings_title": "客户端设置",

      // 启动页
      "splash.subtitle": "桌面客户端",
      "splash.detecting": "正在检测服务…",
      "splash.start": "启动服务",
      "splash.retry": "重试",

      // 服务状态徽标
      "state.none": "未运行",
      "state.starting": "启动中",
      "state.running": "运行中",
      "state.stopped": "已停止",
      "state.error": "启动失败",

      // 设置页：侧栏
      "nav.service": "dsh 服务控制",
      "nav.lan": "局域网代理服务控制",
      "nav.donate": "打赏支持作者",
      "side.check_update": "检查更新",
      "side.src_builtin": "内置运行时",
      "side.src_system": "复用系统运行时",
      "side.version_unknown": "版本未知",
      "side.client": "客户端",

      // 常规设置面板
      "gen.quit": "退出行为",
      "gen.quit.stop": "退出应用时停止服务",
      "gen.quit.stop.desc": "点击系统托盘「退出应用」时，是否一并停止本应用启动的服务。默认不勾选：退出应用后服务继续运行在 127.0.0.1:3080，下次打开应用会自动接管，仍可从托盘停止或重启它。外部已有的服务不受此开关影响。",

      // 隐私（匿名使用统计，默认关闭）
      "gen.privacy": "隐私",
      "gen.privacy.telemetry": "匿名使用统计",
      "gen.privacy.telemetry.desc": "默认关闭。开启后，应用会向作者的自建 Sentry 服务上报崩溃信息与匿名使用事件（如启动、启动/停止服务、配对），用于发现并修复问题。上报内容仅含一个不可逆的匿名机器 ID、应用与工具链版本号，不含任何文件内容、聊天记录、API Key 或个人身份信息。可随时关闭，关闭后立即停止上报。",

      // 服务控制面板
      "svc.title": "dsh 服务控制",
      "svc.unknown": "未知",
      "svc.hint": "关闭窗口不会停止服务；本应用启动的服务可由托盘或此处管理。若 3080 上运行的是外部启动的服务，启动/停止/重启均不可用。",
      "svc.start": "启动 dsh 服务",
      "svc.restart": "重启 dsh 服务",
      "svc.stop": "停止 dsh 服务",
      "svc.log": "服务运行日志",
      "svc.refresh": "刷新",
      "svc.log.empty": "暂无日志：服务尚未启动，或日志文件不存在。",

      // 局域网面板
      "lan.title": "局域网代理服务控制",
      "lan.detect": "检测中…",
      "lan.hint": "手机与电脑连接同一 Wi-Fi，扫码确认一次性配对码后，即可在手机浏览器打开 Harness 界面；本机 Loopback 访问不受影响。配对身份跟随浏览器（Cookie）而非 IP，因此也适用于 localhost.run 等内网穿透隧道——每台设备各自配对，互不影响。",
      "lan.start": "启动局域网代理服务",
      "lan.restart": "重启局域网代理服务",
      "lan.restart.tooltip": "重启局域网代理服务：保留配对码与已配对会话",
      "lan.stop": "停止局域网代理服务",
      "lan.hint2": "配对成功后配对码立即作废，新设备请用窗口中的新码；已配对浏览器 30 分钟内免确认。",
      "lan.code": "一次性配对码",
      "lan.url": "完整访问地址（含配对码）",
      "lan.regen": "重新生成配对码",
      "lan.copy_qr": "复制二维码图片",
      "lan.copy_url": "复制访问链接",
      "lan.devices": "已配对设备",
      "lan.devices.empty": "暂无",
      "lan.log": "代理服务日志",
      "lan.log.empty": "暂无日志：代理服务尚未启动。",
      "lan.status.stopped": "代理已停止",
      "lan.status.running_up": "代理运行中 · dsh 服务在线",
      "lan.status.running_down": "代理运行中 · dsh 服务未运行",
      "lan.session.title": "{0}（扫码配对，剩余 {1} 分钟）",
      "lan.session.tunnel_suffix": "（隧道/本机）",
      "lan.session.tunnel_title": "经内网穿透隧道或本机访问，来源 IP 统一为回环地址，无法用于区分设备",
      "lan.session.ip_title": "该会话配对时的来源 IP",

      // 更新
      "update.checking": "正在检查更新…",
      "update.failed": "检查更新失败，请稍后重试",
      "update.found": "发现客户端新版本 v{0}",
      "update.download": "前往下载",
      "update.ignore": "忽略",
      "update.ignored": "已忽略 v{0} ",
      "update.latest": "客户端已是最新版本 v{0}",

      // 打赏
      "donate.title": "打赏支持作者",
      "donate.hint": "如果您觉得 DeepSeekHarness 帮助到了您，欢迎支持作者继续创作。最推荐的方式：通过作者的邀请链接订阅 OpenCode Go，您与作者各得 $5 订阅额度，双赢！",
      "donate.block.title": "OpenCode Go · 云端 AI 编程订阅",
      "donate.block.desc": "基于开源 opencode.ai 的 Coding Plan 订阅服务。通过作者的邀请链接订阅，您和作者各得 $5 订阅额度——您的订阅既是给自己添一份 AI 编程额度，也是对作者最实在的支持。",
      "donate.cta": "通过邀请链接订阅（双方各得 $5）",
      "donate.quota.title": "订阅套餐额度",
      "donate.quota.hours": "5 小时限制",
      "donate.quota.week": "每周限制",
      "donate.quota.month": "每月限制",
      "donate.quota.hours.v": "$12 使用额度",
      "donate.quota.week.v": "$30 使用额度",
      "donate.quota.month.v": "$60 使用额度",
      "donate.quota.hint": "使用便宜点的模型，几乎不会有 Token 焦虑 ✨",
      "donate.note": "订阅成功后您立即获得对应额度，作者的额度也随之增加。感谢您的支持，让作者有动力持续维护这个开源项目！",

      // dsh 版本管理面板
      "nav.versions": "dsh 版本管理",
      "versions.title": "dsh 版本管理",
      "versions.hint": "管理本客户端使用的 dsh 版本。选择具体版本后，服务启动将直接使用该版本（不经 npx），不再自动更新。",
      "versions.current": "当前版本：",
      "versions.default_bundled": "默认（使用 bundled 运行时）",
      "versions.default_plain": "默认（使用 npx）",
      "versions.clear": "恢复默认",
      "versions.loading": "正在加载版本列表…",
      "versions.refresh": "刷新版本列表",
      "versions.clear_hint_bundled": "恢复默认后将使用内置 bundled 运行时启动服务",
      "versions.clear_hint_plain": "恢复默认后将通过 npx 启动服务",
      "versions.download_note": "版本下载需联网，每个版本约 30–50 MB。",
      "versions.col.version": "版本",
      "versions.col.status": "状态",
      "versions.col.action": "操作",
      "versions.status.builtin": "内置",
      "versions.status.active": "使用中",
      "versions.status.downloaded": "已下载",
      "versions.status.available": "可下载",
      "versions.action.use": "切换使用",
      "versions.action.download": "下载",
      "versions.action.downloading": "下载中…",
      "versions.action.delete": "删除",
      "versions.action.deleting": "删除中…",
      "versions.downloading": "正在下载 DSH {0}…",
      "versions.download_done": "DSH {0} 下载完成",
      "versions.download_error": "DSH {0} 下载失败：{1}（详情见应用日志）",
      "versions.set_active": "已切换到 DSH {0}",
      "versions.set_active_confirm": "确定切换到 DSH {0} 并重启服务？",
      "versions.restart_hint": "重启服务后生效",
      "versions.set_active_title": "切换版本",
      "versions.restart_switch": "立即重启切换",
      "versions.cancel_switch": "取消切换",
      "versions.restarting": "正在重启服务…",
      "versions.confirm_delete": "确定删除 DSH {0}？",
      "versions.confirm_delete_title": "删除确认",
      "common.cancel": "取消",
      "versions.deleted": "已删除",
      "versions.deleting": "正在删除…",
      "versions.cleared": "已恢复默认版本",
      "versions.cleared_confirm": "确定恢复默认版本并重启服务？",
      "versions.active_display": "{0}  ● 使用中",
      "versions.builtin_display": "{0}（内置）",
      "versions.registry.title": "下载源",
      "versions.registry.official": "npm 官方源",
      "versions.registry.mirror": "淘宝镜像源",

      // Toast
      "toast.url_copied": "访问链接已复制到剪贴板",
      "toast.qr_copied": "二维码图片已复制到剪贴板",
      "toast.code_regen": "已重新生成配对码，原设备需重新扫码",
      "toast.proxy_stopped": "代理已停止",
      "toast.proxy_started": "代理已启动",
      "toast.proxy_restarted": "代理已重启",
      "toast.opencode_open": "已打开 OpenCode Go 订阅页",
    },

    en: {
      "win.close": "Close Window",
      "win.close.tooltip": "Close window (hide to tray)",
      "win.min": "Minimize",
      "win.full": "Fullscreen",
      "win.full.tooltip": "Fullscreen / Exit Fullscreen",
      "top.settings": "Settings",
      "top.settings.tooltip": "Open Client Settings",
      "top.refresh": "Refresh",
      "top.detect": "Detecting…",
      "top.show_main": "Show Main Window",
      "win.settings_title": "Settings",

      "splash.subtitle": "Desktop Client",
      "splash.detecting": "Detecting service…",
      "splash.start": "Start Service",
      "splash.retry": "Retry",

      "state.none": "Not Running",
      "state.starting": "Starting",
      "state.running": "Running",
      "state.stopped": "Stopped",
      "state.error": "Start Failed",

      "nav.service": "dsh Service",
      "nav.lan": "LAN Proxy",
      "nav.donate": "Support the Author",
      "side.check_update": "Check for Updates",
      "side.src_builtin": "Bundled runtime",
      "side.src_system": "System runtime",
      "side.version_unknown": "Unknown",
      "side.client": "Client",

      "gen.quit": "On Quit",
      "gen.quit.stop": "Stop service when quitting",
      "gen.quit.stop.desc": "Whether to also stop the service started by this app when Quit is chosen from the tray. Unchecked by default: after quitting, the service keeps running on 127.0.0.1:3080, is taken over automatically the next time the app opens, and can still be stopped or restarted from the tray. Externally started services are never affected.",

      "gen.privacy": "Privacy",
      "gen.privacy.telemetry": "Anonymous usage statistics",
      "gen.privacy.telemetry.desc": "Off by default. When enabled, the app reports crash information and anonymous usage events (such as app start, service start/stop, and pairing) to the author's self-hosted Sentry instance, to help find and fix issues. Reports contain only an irreversible anonymous machine ID plus app/toolchain version numbers — never file contents, chat history, API keys, or any personally identifiable information. You can turn this off at any time; reporting stops immediately.",

      "svc.title": "dsh Service",
      "svc.unknown": "Unknown",
      "svc.hint": "Closing the window does not stop the service; services started by this app can be managed from the tray or here. If the service on port 3080 was started externally, Start/Stop/Restart are all unavailable.",
      "svc.start": "Start dsh Service",
      "svc.restart": "Restart dsh Service",
      "svc.stop": "Stop dsh Service",
      "svc.log": "Service Log",
      "svc.refresh": "Refresh",
      "svc.log.empty": "No log yet: the service has not started, or the log file does not exist.",

      "lan.title": "LAN Proxy Service",
      "lan.detect": "Detecting…",
      "lan.hint": "Connect your phone to the same Wi-Fi, scan the code and confirm the one-time pair code, then open the Harness UI in the phone browser; local loopback access is unaffected. Pairing identity follows the browser (Cookie) rather than IP, so it also works through tunnels like localhost.run — each device pairs independently.",
      "lan.start": "Start LAN Proxy Service",
      "lan.restart": "Restart LAN Proxy Service",
      "lan.restart.tooltip": "Restart LAN proxy: keeps the pair code and paired sessions",
      "lan.stop": "Stop LAN Proxy Service",
      "lan.hint2": "The pair code is invalidated immediately after pairing; new devices must use the fresh code in this window. Paired browsers stay trusted for 30 minutes without re-confirmation.",
      "lan.code": "One-time Pair Code",
      "lan.url": "Full Access URL (with pair code)",
      "lan.regen": "Regenerate Pair Code",
      "lan.copy_qr": "Copy QR Image",
      "lan.copy_url": "Copy Access Link",
      "lan.devices": "Paired Devices",
      "lan.devices.empty": "None",
      "lan.log": "Proxy Log",
      "lan.log.empty": "No log yet: the proxy service has not started.",
      "lan.status.stopped": "Proxy Stopped",
      "lan.status.running_up": "Proxy Running · dsh Service Online",
      "lan.status.running_down": "Proxy Running · dsh Service Not Running",
      "lan.session.title": "{0} (paired by QR, {1} min left)",
      "lan.session.tunnel_suffix": " (tunnel/local)",
      "lan.session.tunnel_title": "Accessed through a tunnel or locally; source IP is a loopback address and cannot distinguish devices",
      "lan.session.ip_title": "Source IP at pairing time",

      "update.checking": "Checking for updates…",
      "update.failed": "Update check failed, please try again later",
      "update.found": "New client version v{0} found",
      "update.download": "Download",
      "update.ignore": "Ignore",
      "update.ignored": "Ignored v{0} ",
      "update.latest": "Client up to date (v{0})",

      "donate.title": "Support the Author",
      "donate.hint": "If DeepSeekHarness has helped you, consider supporting the author. Best way: subscribe to OpenCode Go through the author's invite link — you and the author each get $5 in credit. Win-win!",
      "donate.block.title": "OpenCode Go · Cloud AI Coding Plan",
      "donate.block.desc": "A Coding Plan subscription built on the open-source opencode.ai. Subscribe through the author's invite link and you and the author each get $5 in credit — your subscription adds AI coding credit for yourself and is the most practical support for the author.",
      "donate.cta": "Subscribe via Invite Link (Both Get $5)",
      "donate.quota.title": "Plan Quotas",
      "donate.quota.hours": "5-hour limit",
      "donate.quota.week": "Weekly limit",
      "donate.quota.month": "Monthly limit",
      "donate.quota.hours.v": "$12 credit",
      "donate.quota.week.v": "$30 credit",
      "donate.quota.month.v": "$60 credit",
      "donate.quota.hint": "With cheaper models you rarely worry about tokens ✨",
      "donate.note": "You get the quota immediately after subscribing, and the author's quota grows too. Thank you for supporting the continued maintenance of this open-source project!",

      "nav.versions": "dsh Versions",
      "versions.title": "dsh Version Manager",
      "versions.hint": "Manage the dsh version used by this client. After selecting a specific version, the service will use it directly (bypassing npx) and will not auto-update.",
      "versions.current": "Current version:",
      "versions.default_bundled": "Default (bundled runtime)",
      "versions.default_plain": "Default (npx)",
      "versions.clear": "Restore Default",
      "versions.loading": "Loading version list…",
      "versions.refresh": "Refresh Version List",
      "versions.clear_hint_bundled": "Restore Default will use the bundled runtime",
      "versions.clear_hint_plain": "Restore Default will use npx",
      "versions.download_note": "Downloading requires internet; each version is about 30–50 MB.",
      "versions.col.version": "Version",
      "versions.col.status": "Status",
      "versions.col.action": "Action",
      "versions.status.builtin": "Bundled",
      "versions.status.active": "In Use",
      "versions.status.downloaded": "Downloaded",
      "versions.status.available": "Available",
      "versions.action.use": "Use",
      "versions.action.download": "Download",
      "versions.action.downloading": "Downloading…",
      "versions.action.delete": "Delete",
      "versions.action.deleting": "Deleting…",
      "versions.downloading": "Downloading DSH {0}…",
      "versions.download_done": "DSH {0} downloaded",
      "versions.download_error": "DSH {0} download failed: {1} (see app logs)",
      "versions.set_active": "Switched to DSH {0}",
      "versions.set_active_confirm": "Switch to DSH {0} and restart the service?",
      "versions.restart_hint": "Restart the service to apply",
      "versions.set_active_title": "Switch Version",
      "versions.restart_switch": "Restart & Switch",
      "versions.cancel_switch": "Cancel",
      "versions.restarting": "Restarting service…",
      "versions.confirm_delete": "Delete DSH {0}?",
      "versions.confirm_delete_title": "Confirm Delete",
      "common.cancel": "Cancel",
      "versions.deleted": "Deleted",
      "versions.deleting": "Deleting…",
      "versions.cleared": "Default version restored",
      "versions.cleared_confirm": "Restore default version and restart the service?",
      "versions.active_display": "{0}  ● In Use",
      "versions.builtin_display": "{0} (bundled)",
      "versions.registry.title": "Download Source",
      "versions.registry.official": "npm Official",
      "versions.registry.mirror": "Taobao Mirror",

      "toast.url_copied": "Access link copied to clipboard",
      "toast.qr_copied": "QR image copied to clipboard",
      "toast.code_regen": "Pair code regenerated; previously paired devices must scan again",
      "toast.proxy_stopped": "Proxy stopped",
      "toast.proxy_started": "Proxy started",
      "toast.proxy_restarted": "Proxy restarted",
      "toast.opencode_open": "Opened OpenCode Go subscription page",
    },
  };

  let locale = "zh";
  let ready = false;

  function t(key, vars) {
    const table = DICT[locale] || DICT.zh;
    let s = Object.prototype.hasOwnProperty.call(table, key) ? table[key] : DICT.zh[key];
    if (s === undefined) return key;
    if (vars) {
      for (const [k, v] of Object.entries(vars)) {
        s = s.replace(new RegExp(`\\{${k}\\}`, "g"), String(v));
      }
    }
    return s;
  }

  /** 把 [data-i18n*] 元素的文案/属性替换为当前语言文本。 */
  function applyLocale() {
    document.documentElement.lang = locale === "zh" ? "zh-CN" : "en";
    document.querySelectorAll("[data-i18n]").forEach((el) => {
      el.textContent = t(el.dataset.i18n);
    });
    document.querySelectorAll("[data-i18n-tooltip]").forEach((el) => {
      el.title = t(el.dataset.i18nTooltip);
    });
    document.querySelectorAll("[data-i18n-placeholder]").forEach((el) => {
      el.placeholder = t(el.dataset.i18nPlaceholder);
    });
    document.querySelectorAll("[data-i18n-aria]").forEach((el) => {
      el.setAttribute("aria-label", t(el.dataset.i18nAria));
    });
    // 页面标题（data-doc-title 给出 key，例如设置页 "客户端设置 - DeepSeekHarness"）。
    const docTitle = document.querySelector("[data-doc-title]");
    if (docTitle) {
      const name = t(docTitle.dataset.docTitle);
      document.title = `${name} - DeepSeekHarness`;
    }
  }

  function setLocale(next) {
    next = next === "en" ? "en" : "zh";
    if (!ready || next !== locale) {
      locale = next;
      applyLocale();
      // 通知页面脚本重渲染动态文案（状态徽标 / 局域网面板 / 更新状态等）。
      window.dispatchEvent(new CustomEvent("dsh:locale", { detail: { locale } }));
    }
    ready = true;
  }

  // 初始化：读后端当前语言；监听语言切换（settings.yaml 变更 → Rust 广播）。
  (function boot() {
    const __T = window.__TAURI__;
    if (__T && __T.core) {
      __T.core
        .invoke("get_locale")
        .then((l) => setLocale(String(l || "zh")))
        .catch(() => setLocale("zh"));
      __T.event
        .listen("locale-changed", (e) => setLocale(String((e.payload) || "zh")))
        .catch(() => {});
    } else {
      setLocale("zh");
    }
  })();

  window.t = t;
})();