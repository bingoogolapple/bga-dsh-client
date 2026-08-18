/* Shared helpers for DeepSeekHarness UI (no bundler; window.__TAURI__ is injected by Tauri). */

const DSH_URL = "http://127.0.0.1:3080";

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
  const keys = { none: "state.none", starting: "state.starting", running: "state.running", stopped: "state.stopped", error: "state.error" };
  const key = keys[s];
  if (key && typeof window.t === "function") return window.t(key);
  return { none: "未运行", starting: "启动中", running: "运行中", stopped: "已停止", error: "启动失败" }[s] || s;
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

/* 自定义窗口控制（主窗口与设置窗口共用）：
   关闭 = 收起到托盘/隐藏（不停止服务）；最小化；全屏切换。
   全屏时给 body 加 win-fullscreen，收起圆角与投影（macOS 全屏为直角满屏）。 */
(function () {
  const w = __T && __T.window;
  if (!w || !$("btn-win-close")) return;
  const win = w.getCurrentWindow();
  $("btn-win-close").addEventListener("click", () => win.close().catch(() => {}));
  $("btn-win-min").addEventListener("click", () => win.minimize().catch(() => {}));
  $("btn-win-full").addEventListener("click", () => win.toggleFullscreen().catch(() => {}));
  const syncFullscreen = () =>
    win
      .isFullscreen()
      .then((fs) => document.body.classList.toggle("win-fullscreen", fs))
      .catch(() => {});
  window.addEventListener("resize", syncFullscreen);
  syncFullscreen();
})();