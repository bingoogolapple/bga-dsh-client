/* Shared helpers for DeepSeekHarness UI (no bundler; window.__TAURI__ is injected by Tauri). */
/* eslint-disable no-unused-vars -- 本文件是共享工具模块：下列顶层函数/常量由
   splash.js / settings.js / i18n.js 通过全局作用域调用，在本文件内"只定义未调用"
   是设计使然。每个符号都已逐个 grep 确认被跨文件引用；新增符号请同步维护
   eslint.config.mjs 的 crossFileExports 列表。 */

// Keep development and packaged builds on the exact loopback authority printed
// by dsh. Its authentication cookie is authority-bound, so changing only the
// packaged build to `localhost` creates a separate browser session.
const DSH_URL = "http://127.0.0.1:3080";

if (/Windows/i.test(navigator.userAgent)) {
  document.body.classList.add("platform-windows");
}

/** 确认对话框的静态 DOM 骨架（纯字面量，不含任何外部数据）。
 *  所有动态文本都在创建后通过 textContent 写入，见 confirmDialog()。 */
const CONFIRM_DIALOG_HTML =
  '<div class="dsh-confirm-box" role="dialog" aria-modal="true">' +
  '<div class="dsh-confirm-title"></div>' +
  '<div class="dsh-confirm-msg"></div>' +
  '<div class="dsh-confirm-detail hidden"></div>' +
  '<div class="dsh-confirm-actions">' +
  '<button class="dsh-confirm-cancel" type="button"></button>' +
  '<button class="dsh-confirm-ok" type="button"></button>' +
  "</div></div>";

const __T = window.__TAURI__;

/** Call a Rust command. */
function invoke(cmd, args) {
  return __T.core.invoke(cmd, args || {});
}

/** Subscribe to a Rust event. */
function listen(event, cb) {
  return __T.event.listen(event, cb);
}

function $(id) {
  return document.getElementById(id);
}

/** 服务状态徽标文案（随当前语言）。 */
function stateLabel(s) {
  const keys = {
    none: "state.none",
    starting: "state.starting",
    running: "state.running",
    stopped: "state.stopped",
    error: "state.error",
  };
  const key = keys[s];
  if (key && typeof window.t === "function") return window.t(key);
  return (
    { none: "未运行", starting: "启动中", running: "运行中", stopped: "已停止", error: "启动失败" }[
      s
    ] || s
  );
}

function toast(msg) {
  let el = $("toast");
  if (!el) {
    el = document.createElement("div");
    el.id = "toast";
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.add("show");
  clearTimeout(el._t);
  el._t = setTimeout(() => el.classList.remove("show"), 2600);
}

/**
 * 轻量 DOM 确认弹窗。Tauri v2 已移除 webview 中的原生 window.confirm/alert/prompt，
 * 直接调用会返回 undefined 导致逻辑被静默跳过（例如删除按钮"点击无反应"）。
 * 返回一个 Promise<boolean>，点击确认 resolve(true)，取消/关闭 resolve(false)。
 */
function confirmDialog(message, opts) {
  opts = opts || {};
  const title = opts.title || "请确认";
  const detail = opts.detail || "";
  const okText = opts.okText || "确定";
  const cancelText = opts.cancelText || "取消";
  return new Promise((resolve) => {
    let overlay = $("dsh-confirm-overlay");
    if (!overlay) {
      overlay = document.createElement("div");
      overlay.id = "dsh-confirm-overlay";
      overlay.className = "dsh-confirm-overlay hidden";
      // 纯静态骨架：单个字面量常量，不含任何变量；下方所有动态内容
      // （title / message / detail / 按钮文案）均通过 textContent 写入。
      overlay.innerHTML = CONFIRM_DIALOG_HTML;
      document.body.appendChild(overlay);
    }
    overlay.querySelector(".dsh-confirm-title").textContent = title;
    overlay.querySelector(".dsh-confirm-msg").textContent = message;
    const detailEl = overlay.querySelector(".dsh-confirm-detail");
    if (detail) {
      detailEl.textContent = detail;
      detailEl.classList.remove("hidden");
    } else {
      detailEl.classList.add("hidden");
    }
    overlay.querySelector(".dsh-confirm-ok").textContent = okText;
    overlay.querySelector(".dsh-confirm-cancel").textContent = cancelText;

    const okBtn = overlay.querySelector(".dsh-confirm-ok");
    const cancelBtn = overlay.querySelector(".dsh-confirm-cancel");
    const close = (val) => {
      overlay.classList.add("hidden");
      okBtn.removeEventListener("click", onOk);
      cancelBtn.removeEventListener("click", onCancel);
      overlay.removeEventListener("click", onBackdrop);
      resolve(val);
    };
    const onOk = () => close(true);
    const onCancel = () => close(false);
    const onBackdrop = (e) => {
      if (e.target === overlay) close(false);
    };
    okBtn.addEventListener("click", onOk);
    cancelBtn.addEventListener("click", onCancel);
    overlay.addEventListener("click", onBackdrop);
    overlay.classList.remove("hidden");
    okBtn.focus();
  });
}

/* 自定义窗口控制（主窗口与设置窗口共用）：
   关闭 = 收起到托盘/隐藏（不停止服务）；最小化；全屏切换。
   全屏时给 body 加 win-fullscreen，收起圆角与投影（macOS 全屏为直角满屏）。 */
(function () {
  const w = __T && __T.window;
  if (!w || !$("btn-win-close")) return;
  const win = w.getCurrentWindow();
  $("btn-win-close").addEventListener("click", () => win.close().catch(() => {}));
  $("btn-win-min").addEventListener("click", () => win.minimize().catch(() => {}));
  $("btn-win-full").addEventListener("click", async () => {
    try {
      const fullscreen = await win.isFullscreen();
      await win.setFullscreen(!fullscreen);
      document.body.classList.toggle("win-fullscreen", !fullscreen);
    } catch (error) {
      console.error("Failed to toggle fullscreen", error);
    }
  });
  const syncFullscreen = () =>
    win
      .isFullscreen()
      .then((fs) => document.body.classList.toggle("win-fullscreen", fs))
      .catch(() => {});
  window.addEventListener("resize", syncFullscreen);
  syncFullscreen();
})();
