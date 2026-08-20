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
      "gen.method": "服务拉起方式",
      "gen.method.hint": "选择应用启动（或托盘「启动服务 / 重启服务」）时如何拉起 DSH 服务。",
      "gen.method.builtin_note": "内置版：DSH 服务由应用自带的 Node.js 与 dsh 拉起（离线可用），无需配置。",
      "gen.method.npx": "方式一（默认）· npx 拉起",
      "gen.method.npx.desc": "无需全局安装，首次运行会自动下载。",
      "gen.method.dsh": "方式二 · 全局命令拉起",
      "gen.method.dsh.desc": "已全局安装过 @deepseek-ai/dsh 时使用。",
      "gen.method.pnpm": "方式三 · 指定目录 + pnpm 拉起",
      "gen.method.pnpm.desc": "进入指定目录（DeepSeekHarness 源码目录）后执行。",
      "gen.dir.placeholder": "选择 DeepSeekHarness 源码目录，例如 ~/dsh/deepseek-harness",
      "gen.pick_dir": "选择目录…",
      "gen.quit": "退出行为",
      "gen.quit.stop": "退出应用时停止服务",
      "gen.quit.stop.desc": "点击系统托盘「退出应用」时，是否一并停止本应用启动的服务。默认不勾选：退出应用后服务继续运行在 127.0.0.1:3080，下次打开应用会自动接管，仍可从托盘停止或重启它。外部已有的服务不受此开关影响。",

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

      "gen.method": "How to launch the service",
      "gen.method.hint": "Choose how the DSH service is launched when the app starts (or via tray items Start/Restart).",
      "gen.method.builtin_note": "Bundled build: the DSH service is launched with the included Node.js and dsh (offline-ready); nothing to configure.",
      "gen.method.npx": "Method 1 (default) · npx",
      "gen.method.npx.desc": "No global install needed; it downloads automatically on first run.",
      "gen.method.dsh": "Method 2 · Global command",
      "gen.method.dsh.desc": "Use when @deepseek-ai/dsh is installed globally.",
      "gen.method.pnpm": "Method 3 · Directory + pnpm",
      "gen.method.pnpm.desc": "Runs in the chosen directory (the DeepSeekHarness source dir).",
      "gen.dir.placeholder": "Choose the DeepSeekHarness source directory, e.g. ~/dsh/deepseek-harness",
      "gen.pick_dir": "Choose Directory…",
      "gen.quit": "On Quit",
      "gen.quit.stop": "Stop service when quitting",
      "gen.quit.stop.desc": "Whether to also stop the service started by this app when Quit is chosen from the tray. Unchecked by default: after quitting, the service keeps running on 127.0.0.1:3080, is taken over automatically the next time the app opens, and can still be stopped or restarted from the tray. Externally started services are never affected.",

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
  window.applyLocale = applyLocale;
  window.__DSH_LOCALE__ = () => locale;
})();