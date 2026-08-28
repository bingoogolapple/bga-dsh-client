// ESLint 9 flat config。
//
// ui/ 是**无打包器**的纯静态前端：脚本以普通 <script> 引入，靠 window 上的
// 全局变量互相通信（window.__TAURI__ 由 Tauri 注入，t/__openPanel 等由 i18n.js
// 挂到 window）。因此：
// - sourceType 用 "script"（非 module），避免把顶层 const 误判为模块作用域；
// - 大量 browser 全局变量与跨文件共享的 window 全局需在 globals 里声明，
//   否则 no-undef 会满屏报错。

import globals from "globals";

/**
 * 被其他文件消费、但在本文件内"只定义未使用"的跨文件全局。
 *
 * ui/ 用普通 <script> 顺序加载、共享同一个全局作用域：app.js 里定义的
 * invoke/listen/$/toast 等由 splash.js / settings.js / i18n.js 使用。
 * ESLint 按单文件分析，会把它们报成 no-unused-vars。
 *
 * 这里只对**已确认跨文件被引用**的符号开白名单（逐个 grep 验证过）；
 * 真正的死代码（如 splash.js 里曾经的 lastInfo）仍会被拦截。
 */
const crossFileExports = [
  // app.js 定义 → 被 splash.js / settings.js / i18n.js 使用
  "invoke",
  "listen",
  "$",
  "stateLabel",
  "toast",
  "confirmDialog",
  "DSH_URL",
  // i18n.js 定义 → 被其他文件使用
  "t",
  "setLocale",
  "currentLocale",
  "applyI18n",
  // settings.js 定义 → 供 Rust 侧 w.eval("window.__openPanel(...)") 调用
  "__openPanel",
];

const sharedGlobals = {
  // Tauri v2 注入（withGlobalTauri: true）
  __TAURI__: "readonly",
  ...Object.fromEntries(crossFileExports.map((name) => [name, "readonly"])),
};

export default [
  {
    files: ["ui/assets/*.js"],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "script",
      globals: {
        ...globals.browser,
        ...sharedGlobals,
      },
    },
    linterOptions: {
      reportUnusedDisableDirectives: true,
    },
    rules: {
      // 错误级：真的会导致 bug 的写法
      "no-undef": "error",
      "no-unused-vars": ["error", { args: "none", caughtErrors: "none", varsIgnorePattern: "^_" }],
      "no-dupe-keys": "error",
      "no-dupe-args": "error",
      "no-unreachable": "error",
      "no-constant-condition": "error",
      "no-self-compare": "error",
      "no-cond-assign": "error",
      "no-fallthrough": "error",
      "no-sparse-arrays": "error",
      "use-isnan": "error",
      "valid-typeof": "error",

      // 警告级：代码质量提示，不阻塞构建
      eqeqeq: ["warn", "smart"],
      "no-var": "warn",
      "prefer-const": "warn",
      "no-console": "off", // 桌面壳里 console 是有用的调试输出，不限制
      "no-implied-eval": "warn",

      // 安全性：本项目会把日志/版本号/registry 等字符串渲染进 DOM，
      // 禁止 eval 与 Function 构造器（配合 CSP 的 script-src 'self' 形成纵深防御）。
      "no-eval": "error",
      "no-new-func": "error",
      "no-script-url": "error",
      // 禁止给 innerHTML/outerHTML 赋**含变量的**模板字符串或拼接表达式——
      // 历史上 settings.js 的已配对设备列表正是用 innerHTML 拼接 `${s.ip}`
      // （来源 IP 为网络侧数据）造成 XSS 隐患。改为 DOM API 构建后，
      // 这里加规则防止回退。
      //
      // 刻意放行两类安全用法（否则误报太多、规则会被整体关掉）：
      // 1) 纯字面量拼接（app.js 里 confirm 对话框的静态结构）；
      // 2) 清空内容的 `el.innerHTML = ""` / `el.innerHTML = cond ? x : ""`。
      // 另：settings.js 的 `pair-qr` 必须走 innerHTML（Rust qrcodegen 生成的
      // SVG 字符串需作为标记插入），该变量为本地生成的可信 SVG，已在文件内
      // 用行内 disable 标注。
      "no-restricted-syntax": [
        "error",
        {
          selector:
            "AssignmentExpression[left.property.name=/^(innerHTML|outerHTML)$/] > TemplateLiteral.right",
          message:
            "不要用 innerHTML 拼接模板字符串（XSS 风险）。改为 textContent 或 createElement + appendChild；清空内容请用 replaceChildren()。",
        },
        {
          selector:
            "AssignmentExpression[left.property.name=/^(innerHTML|outerHTML)$/] > BinaryExpression.right",
          message:
            "不要用 innerHTML 拼接变量（XSS 风险）。改为 textContent 或 createElement + appendChild。",
        },
      ],
    },
  },
  {
    // scripts/ 是 Node 环境下的构建脚本（.mjs，ESM）
    files: ["scripts/*.mjs"],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "module",
      globals: globals.node,
    },
    rules: {
      "no-undef": "error",
      "no-unused-vars": ["error", { args: "none" }],
      "no-eval": "error",
      "no-new-func": "error",
    },
  },
];
