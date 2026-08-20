// 打包内置运行时：下载官方 Node 二进制（校验 SHA-256）并 npm 安装 @deepseek-ai/dsh + pnpm，
// 输出 src-tauri/resources/runtime/（nd / rt / runtime-manifest.json），供内置版离线使用。
import { execFileSync } from 'node:child_process'
import { createWriteStream, existsSync, mkdirSync, rmSync, readFileSync, writeFileSync, readdirSync, realpathSync, renameSync } from 'node:fs'
import { join, dirname, basename, relative } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createHash } from 'node:crypto'
import { pipeline } from 'node:stream/promises'
import { Readable } from 'node:stream'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
// 内置运行时版本：全仓唯一定义点（build-release.sh 与 CI 按行提取）。
// 升版本请用 ./scripts/set-runtime-version.sh。
const NODE_VER = 'v24.19.0'
const DSH_VERSION = '0.1.0-rc.8'
const PNPM_VERSION = '11.22.0'

// 交叉捆绑：RUNTIME_TARGET=win32|linux 时在异构主机上为指定平台组装运行时
// （下载对平台 zip/tar.gz + npm --os/--cpu 按目标平台解析 optionalDependencies 与
// prebuilds）。官方路径仍是目标平台机器上跑本机 bundle-runtime（不设 RUNTIME_TARGET）。
// 可选 TARGET_ARCH=arm64 交叉 linux-arm64（默认 x64）。
const TARGET = process.env.RUNTIME_TARGET
if (TARGET && !['win32', 'linux'].includes(TARGET)) {
  throw new Error(`RUNTIME_TARGET 仅支持 win32 / linux（当前: ${TARGET}）`)
}
const ARCH = TARGET ? (process.env.TARGET_ARCH || 'x64') : process.arch === 'arm64' ? 'arm64' : 'x64'
const IS_WIN = (TARGET ?? process.platform) === 'win32'
// Windows 官方分发为 zip（含顶层目录 node-v24.19.0-win-x64/）；darwin/linux 为 tar.gz（bin/node，需 strip）
const PLAT = (TARGET ?? process.platform) === 'darwin' ? 'darwin' : IS_WIN ? 'win' : 'linux'
const TARBALL = `node-${NODE_VER}-${PLAT}-${ARCH}.${IS_WIN ? 'zip' : 'tar.gz'}`
const URL = `https://nodejs.org/dist/${NODE_VER}/${TARBALL}`
const SHASUMS_URL = `https://nodejs.org/dist/${NODE_VER}/SHASUMS256.txt`

// Tauri bundle.resources 相对 src-tauri；运行时根目录必须以 runtime 子目录命名
const runtimeRoot = join(root, 'src-tauri', 'resources', 'runtime')
const nodeDir = join(runtimeRoot, 'nd')
const runtimeDir = join(runtimeRoot, 'rt')
mkdirSync(nodeDir, { recursive: true })
mkdirSync(runtimeDir, { recursive: true })

// 1) 下载并校验 Node 官方 SHA-256（已存在的 tarball 也强制复核）
const tarPath = join(nodeDir, TARBALL)
async function verifyNodeSha256(file) {
  const sums = await fetch(SHASUMS_URL)
  if (!sums.ok) throw new Error(`SHASUMS 下载失败: ${sums.status}`)
  const sumsText = await sums.text()
  const expected = sumsText
    .split('\n')
    .find((line) => line.trim().endsWith(`  ${TARBALL}`) || line.trim().endsWith(` *${TARBALL}`))
    ?.split(/\s+/)[0]
  if (!expected) throw new Error(`SHASUMS256.txt 中找不到 ${TARBALL}`)
  const actual = createHash('sha256').update(readFileSync(file)).digest('hex')
  if (actual !== expected.toLowerCase()) {
    throw new Error(`Node tarball SHA-256 校验失败：期望 ${expected}，实际 ${actual}`)
  }
  console.log('[bundle] Node SHA-256 校验通过')
}
if (!existsSync(tarPath)) {
  console.log(`[bundle] 下载 Node ${NODE_VER} (${PLAT}-${ARCH})… ${URL}`)
  const res = await fetch(URL)
  if (!res.ok || !res.body) throw new Error(`下载失败: ${res.status}`)
  const tmpTar = `${tarPath}.part`
  await pipeline(Readable.fromWeb(res.body), createWriteStream(tmpTar))
  await verifyNodeSha256(tmpTar)
  rmSync(tarPath, { force: true })
  renameSync(tmpTar, tarPath)
} else {
  console.log('[bundle] Node tarball 已存在，复核官方 SHASUM…')
  await verifyNodeSha256(tarPath)
}

// 2) 解压 Node：清理旧产物但保留 zip 缓存
console.log('[bundle] 解压 Node…')
for (const f of readdirSync(nodeDir)) {
  if (f === basename(tarPath)) continue
  rmSync(join(nodeDir, f), { recursive: true, force: true })
}
if (IS_WIN) {
  // Windows 官方分发为 zip（含顶层目录 node-v24.10.0-win-<arch>/，与 tar.gz 同构）。
  // 注意：GitHub Actions 的 Git Bash 里 PATH 优先的是 MSYS GNU tar，它会把 `D:\`
  // 盘符误解析为远程主机（"Cannot connect to D:"），因此 Windows 本机必须显式调用
  // 系统自带 bsdtar（System32\tar.exe，支持解 zip + --strip-components）。
  // 交叉捆绑（在 mac/linux 上 TARGET=win32）路径为正斜杠，走 PATH 里的 tar 即可。
  const tarBin =
    process.platform === 'win32'
      ? join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe')
      : 'tar'
  execFileSync(tarBin, ['-xf', tarPath, '-C', nodeDir, '--strip-components=1'], { stdio: 'inherit' })
} else {
  execFileSync('tar', ['-xzf', tarPath, '-C', nodeDir, '--strip-components=1'], { stdio: 'inherit' })
}

const nodeBin = IS_WIN ? join(nodeDir, 'node.exe') : join(nodeDir, 'bin', 'node')
if (!existsSync(nodeBin)) throw new Error('Node 解压失败')

// 3) 可复现安装：提交的 package.json + lockfile 精确锁定 dsh 与 pnpm，npm ci 安装
const hasLock = existsSync(join(runtimeDir, 'package-lock.json'))
writeFileSync(
  join(runtimeDir, 'package.json'),
  JSON.stringify({ name: 'dsh-client-runtime', private: true, dependencies: { '@deepseek-ai/dsh': DSH_VERSION, pnpm: PNPM_VERSION } }, null, 2) + '\n',
)
console.log(`[bundle] npm ci @deepseek-ai/dsh@${DSH_VERSION} + pnpm@${PNPM_VERSION}（--ignore-scripts）…`)
const npmCli = IS_WIN
  ? join(nodeDir, 'node_modules', 'npm', 'bin', 'npm-cli.js')
  : join(nodeDir, 'lib', 'node_modules', 'npm', 'bin', 'npm-cli.js')
// 交叉捆绑时用本机 node 执行 npm-cli.js（node.exe 无法在 mac 上运行）；本机模式用捆绑 node
const npmRunner = TARGET ? process.execPath : nodeBin
const targetFlags = TARGET ? ['--os=' + (TARGET === 'win32' ? 'win32' : TARGET), '--cpu=' + ARCH] : []
// macOS runner 上 v8 默认堆上限仅 ~2GB，npm 安装 dsh 依赖树时会 OOM
// （Ineffective mark-compacts near heap limit），故显式提到 4GB（与
// Ubuntu 16GB runner 的 v8 动态上限相当；机器内存足够，堆不会真正用满）。
execFileSync(
  npmRunner,
  [npmCli, hasLock ? 'ci' : 'install', '--prefix', runtimeDir, '--no-audit', '--no-fund', '--ignore-scripts', ...targetFlags],
  { stdio: 'inherit', env: { ...process.env, NODE_OPTIONS: '--max-old-space-size=4096' } },
)

const dshBin = join(runtimeDir, 'node_modules', '.bin', 'dsh')
const dshBinWin = join(runtimeDir, 'node_modules', '.bin', 'dsh.cmd')
const pnpmBin = join(runtimeDir, 'node_modules', '.bin', 'pnpm')
const pnpmBinWin = join(runtimeDir, 'node_modules', '.bin', 'pnpm.cmd')
if (!existsSync(dshBin) && !existsSync(dshBinWin)) throw new Error('dsh 安装失败')
if (!existsSync(pnpmBin) && !existsSync(pnpmBinWin)) throw new Error('pnpm 安装失败')
// dsh 的 lib/bin.js 是 node 直接执行入口（hub 同样布局）
const binJs = join(runtimeDir, 'node_modules', '@deepseek-ai', 'dsh', 'lib', 'bin.js')
if (!existsSync(binJs)) throw new Error(`找不到 dsh lib/bin.js：${binJs}`)
console.log(`[bundle] OK: node=${nodeBin} dsh=${binJs}`)

// 交叉捆绑 Windows 专用：非 Windows 生成的 .bin 是 POSIX shim（无 .cmd），
// Windows 下 cmd 找 .cmd/.exe 会 127，为每个无扩展名条目生成同名 .cmd（node "%~dp0<rel目标>" %*）。
// （linux 交叉目标不需要 .cmd shim。）
if (TARGET === 'win32') {
  const binDir = join(runtimeDir, 'node_modules', '.bin')
  let added = 0
  for (const name of readdirSync(binDir)) {
    if (name.includes('.')) continue
    const shimPath = join(binDir, name)
    let target
    try {
      target = realpathSync(shimPath)
    } catch {
      continue
    }
    const rel = relative(binDir, target).replace(/\//g, '\\')
    const cmd = `@ECHO off\r\nSETLOCAL\r\nnode \"%~dp0${rel}\" %*\r\n`
    writeFileSync(join(binDir, `${name}.cmd`), cmd)
    added += 1
  }
  console.log(`[bundle] 交叉捆绑：为 ${added} 个 .bin 条目生成 Windows .cmd shim`)
}

// 4) 运行时 manifest：受控构建输入
const manifest = {
  nodeVersion: NODE_VER,
  nodeTarball: TARBALL,
  nodeSha256: createHash('sha256').update(readFileSync(tarPath)).digest('hex'),
  dshVersion: DSH_VERSION,
  pnpmVersion: PNPM_VERSION,
  platform: TARGET ?? process.platform,
  arch: ARCH,
  generatedAt: new Date().toISOString(),
}
writeFileSync(join(runtimeRoot, 'runtime-manifest.json'), JSON.stringify(manifest, null, 2) + '\n')
console.log(`[bundle] manifest: ${join(runtimeRoot, 'runtime-manifest.json')}`)

// 5) 瘦身（可选，默认开）：删测试/文档/类型/源码映射，压低安装包体积
const TRIM = process.env.TRIM_RUNTIME !== '0'
const SKIP_DIRS = new Set(['test', 'tests', '__tests__', 'benchmark', 'bench', 'examples', 'docs'])

function walkRuntime(dir, skipDirs, fn) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name)
    if (e.isDirectory()) {
      if (e.name !== 'node_modules' && skipDirs.has(e.name)) {
        rmSync(p, { recursive: true, force: true })
        continue
      }
      walkRuntime(p, skipDirs, fn)
    } else fn(p, e.name)
  }
}

if (TRIM) {
  console.log('[bundle] 瘦身 runtime…')
  const rm = (p) => rmSync(p, { recursive: true, force: true })
  const dropFile = (p, name) => {
    if (name.endsWith('.map') || name.endsWith('.d.ts') || name.endsWith('.md')) return rm(p)
    if (/^(README|CHANGELOG|SECURITY|CONTRIBUTING)/i.test(name)) return rm(p)
    if (/^\.(npmignore|gitignore|gitattributes|editorconfig|eslintrc|prettierrc|yarn-integrity|DS_Store)/.test(name)) return rm(p)
  }
  walkRuntime(nodeDir, SKIP_DIRS, dropFile)
  walkRuntime(runtimeDir, SKIP_DIRS, dropFile)

  // Node 分发裁剪：C 头文件、man、解压根（tarball 仅作下载缓存，打包必须排除）、corepack
  for (const p of ['include', 'share']) rm(join(nodeDir, p))
  rm(join(nodeDir, tarPath ? basename(tarPath) : ''))
  rm(join(nodeDir, 'bin', 'corepack'))
  rm(join(nodeDir, 'lib', 'node_modules', 'corepack'))

  // dsh 依赖裁剪：嵌套 @opentelemetry 副本、esnext 构建产物（非 Node exports 条件）、
  // mistralai 的 TS 源码（运行时走 esm 构建）、sharp 的 wasm 备用实现（走本机 prebuilds）
  const nm = join(runtimeDir, 'node_modules')
  const dropNestedOtel = (dir) => {
    for (const e of readdirSync(dir, { withFileTypes: true })) {
      if (!e.isDirectory()) continue
      const p = join(dir, e.name)
      if (e.name === 'node_modules') {
        const otel = join(p, '@opentelemetry')
        if (existsSync(otel) && p !== nm) rm(otel)
        dropNestedOtel(p)
      } else {
        dropNestedOtel(p)
      }
    }
  }
  dropNestedOtel(nm)
  const otelRoot = join(nm, '@opentelemetry')
  if (existsSync(otelRoot)) {
    for (const name of readdirSync(otelRoot)) {
      rm(join(otelRoot, name, 'build', 'esnext'))
    }
  }
  rm(join(nm, '@mistralai', 'mistralai', 'src'))
  rm(join(nm, '@img', 'sharp-wasm32'))

  // 二次裁剪：
  // - pnpm/artifacts 是 standalone exe 打包副本（18M），运行 dist/pnpm.mjs 不需要
  // - node-pty 的 prebuilds 只保留支持平台：darwin 两架构 + win32-x64（交叉目标）
  //   + linux-x64/arm64（交叉目标 TARGET_ARCH）；删 win32-arm64（未支持）
  rm(join(nm, 'pnpm', 'artifacts'))
  const ptyPre = join(nm, 'node-pty', 'prebuilds')
  if (existsSync(ptyPre)) {
    for (const name of readdirSync(ptyPre)) {
      if (!/^(darwin-arm64|darwin-x64|win32-x64|linux-x64|linux-arm64)$/.test(name)) rm(join(ptyPre, name))
    }
  }
}

console.log(`[bundle] 完成：${runtimeRoot}`)