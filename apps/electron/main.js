

const { app, BrowserWindow, Menu, ipcMain, shell, dialog, nativeTheme } = require('electron');
const { spawn } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');
const readline = require('node:readline');

const PRODUCT = 'FrameGen Manager';
// 网盘下载（发布页直链之外的备用下载方式，放在更新窗口与「关于」页）
const NETDISK_URL = 'https://pan.quark.cn/s/adea7ed4f2b2';
// 1.7.0 停更公告里的新项目入口
const PROJECT_NEXT_URL = 'https://github.com/XiaoQAQ10086/FrameGen-Manager-Next';
const PROJECT_NEXT_RELEASES_URL = 'https://github.com/XiaoQAQ10086/FrameGen-Manager-Next/releases';
const RETIRE_URLS = new Set([PROJECT_NEXT_URL, PROJECT_NEXT_RELEASES_URL, NETDISK_URL]);

const DEV = !app.isPackaged || process.env.FGM_DEV === '1';
const USE_GPU = !(process.argv.includes('--fgm-no-gpu') || process.env.FGM_NO_GPU === '1');
function blurAllowed() {
  if (process.env.FGM_NO_BLUR === '1') return false;
  if (process.env.FGM_FORCE_BLUR === '1') return true;
  try {
    const st = app.getGPUFeatureStatus();
    return !!(st && st.gpu_compositing === 'enabled');
  } catch (e) { return false; }
}
if (!USE_GPU) {
  app.commandLine.appendSwitch('disable-gpu');
  app.commandLine.appendSwitch('disable-gpu-compositing');
  app.commandLine.appendSwitch('disable-direct-composition');
  app.disableHardwareAcceleration();
}


const exeDir = app.isPackaged
  ? path.dirname(app.getPath('exe'))
  : path.join(__dirname, '..', '..', '..', 'test-tauri');

const INSTALLED_MARKER = path.join(exeDir, 'installed.txt');
const isInstalled = fs.existsSync(INSTALLED_MARKER);
const appDataRoot = path.join(app.getPath('appData'), 'FrameGen-Manager');

let dataDir = process.env.FGM_DATA_DIR;
if (!dataDir) dataDir = isInstalled ? appDataRoot : exeDir;

function migrateDataIfNeeded() {
  if (!isInstalled || dataDir === exeDir) return;
  const items = ['framegen-manager.json', 'game_library.json', 'tips.md', 'assets', 'backups', 'logs'];
  const srcHasData = items.some(function (n) { return fs.existsSync(path.join(exeDir, n)); });
  const dstHasData = fs.existsSync(path.join(dataDir, 'framegen-manager.json'));
  if (!srcHasData || dstHasData) return;
  fs.mkdirSync(dataDir, { recursive: true });
  const copied = [];
  for (const n of items) {
    const s = path.join(exeDir, n), t = path.join(dataDir, n);
    if (!fs.existsSync(s) || fs.existsSync(t)) continue;
    try {
      fs.cpSync(s, t, { recursive: true, errorOnExist: false });
      copied.push(n);
    } catch (e) {
      console.error('[migrate] 复制失败 ' + n + ': ' + e.message);
    }
  }
  for (const n of copied) {
    try { fs.rmSync(path.join(exeDir, n), { recursive: true, force: true }); } catch (e) {  }
  }
  if (copied.length) console.log('[migrate] 数据已搬到 ' + dataDir + '：' + copied.join('、'));
}

const SIDECAR = (function () {
  const named = path.join(exeDir, 'FrameGen Manager Sidecar.exe');
  if (fs.existsSync(named)) return named;
  const legacy = path.join(exeDir, 'framegen-sidecar.exe');
  return fs.existsSync(legacy) ? legacy : named;
})();

const SHELL_LOG_MAX = 512 * 1024;
function shellLog(msg) {
  try {
    const dir = path.join(dataDir, 'logs');
    fs.mkdirSync(dir, { recursive: true });
    const file = path.join(dir, 'shell.log');
    try {
      if (fs.statSync(file).size > SHELL_LOG_MAX) fs.renameSync(file, path.join(dir, 'shell.prev.log'));
    } catch (e) {  }
    const t = new Date().toTimeString().slice(0, 8);
    fs.appendFileSync(file, '[' + t + '] ' + msg + '\n');
  } catch (e) {  }
}

let sidecar = null;
let sidecarDead = false;
let sidecarStarting = null;
let nextId = 1;
const pending = new Map();
let selfUpdateBusy = false;
// 最近一次「发现新版本」的版本号：界面若漏传 version，主进程兜底补上。
let lastUpdateOffer = null;
let mainWin = null;
let quitting = false;

function startSidecar() {
  if (sidecar && !sidecarDead) return sidecar;
  if (!fs.existsSync(SIDECAR)) {
    sidecarDead = true;
    shellLog('找不到后台进程：' + SIDECAR);
    dialog.showErrorBox(PRODUCT, '找不到后台进程：' + SIDECAR +
      '\n\n安装包不完整或被安全软件删掉了，请重新安装本程序。');
    return null;
  }
  sidecarDead = false;
  const p = spawn(SIDECAR, [], {
    cwd: exeDir,
    stdio: ['pipe', 'pipe', 'pipe'],
    windowsHide: true,
    env: Object.assign({}, process.env, { FGM_DATA_DIR: dataDir }),
  });
  shellLog('启动后台进程：' + SIDECAR);

  const rl = readline.createInterface({ input: p.stdout });
  rl.on('line', onSidecarLine);
  p.stderr.on('data', function (d) {
    const text = String(d).replace(/\s+$/, '');
    console.error('[sidecar]', text);
    shellLog('[stderr] ' + text);
  });
  p.on('error', function (e) {
    sidecarDead = true;
    shellLog('后台进程启动失败：' + e.message);
    for (const [, q] of pending) { clearTimeout(q.timer); q.reject(new Error('后台进程启动失败：' + e.message)); }
    pending.clear();
  });
  p.on('exit', function (code, signal) {
    sidecarDead = true;
    shellLog('后台进程退出：code=' + code + ' signal=' + (signal || '-') +
      (quitting ? '（程序正在退出，正常）' : ''));
    if (quitting) {
      for (const [, q] of pending) { clearTimeout(q.timer); q.reject(new Error('程序正在退出')); }
      pending.clear();
      return;
    }
    const waiting = Array.from(pending.values());
    pending.clear();
    if (!waiting.length) return;
    ensureSidecar().then(function () {
      waiting.forEach(function (q) { if (q.retry) q.retry(); else q.reject(new Error('后台进程已退出（code ' + code + '）')); });
    }, function (e) {
      waiting.forEach(function (q) { clearTimeout(q.timer); q.reject(new Error('后台进程已退出（code ' + code + '）：' + e.message)); });
    });
  });
  return p;
}

function ensureSidecar() {
  if (sidecar && !sidecarDead) return Promise.resolve();
  if (sidecarStarting) return sidecarStarting;
  sidecarStarting = new Promise(function (resolve, reject) {
    sidecar = startSidecar();
    if (!sidecar) { reject(new Error('找不到后台进程：' + SIDECAR)); return; }
    const id = nextId++;
    const timer = setTimeout(function () {
      pending.delete(id);
      reject(new Error('后台进程拉起后没有应答（5 秒超时）'));
    }, 5000);
    pending.set(id, { resolve: function () { resolve(); }, reject: function (e) { reject(e); }, timer: timer });
    try {
      sidecar.stdin.write(JSON.stringify({ id: id, cmd: 'ping', args: {} }) + '\n');
    } catch (e) {
      pending.delete(id);
      clearTimeout(timer);
      reject(e);
    }
  }).then(function (v) { sidecarStarting = null; return v; }, function (e) { sidecarStarting = null; throw e; });
  return sidecarStarting;
}

function onSidecarLine(line) {
  let msg;
  try { msg = JSON.parse(line); } catch (e) { return; }
  if (msg.event === 'progress') {
    if (mainWin && !mainWin.isDestroyed()) {
      mainWin.webContents.send(selfUpdateBusy ? 'fgm:event:selfupdate' : 'fgm:event:progress', msg.payload || {});
    }
    return;
  }
  const p = pending.get(msg.id);
  if (!p) return;
  pending.delete(msg.id);
  clearTimeout(p.timer);
  if (msg.ok) p.resolve(msg.result);
  else {
    // 错误码（sidecar 给的 DRV-001 / FILE-002…）拼在消息末尾：用户截图即带定位信息
    const code = msg.code || 'GEN-999';
    const err = new Error((msg.error || '命令失败') + ' [' + code + ']');
    err.code = code;
    p.reject(err);
  }
}

const CALL_TIMEOUT_MS = 30 * 60 * 1000;

// 后台进程崩掉后**只自动重试只读命令**：查询重发一遍没有副作用；
// 部署 / 还原 / 清理这类写命令重放不可控（上次做到一半），让用户自己再点一次才稳妥。
const RETRYABLE = new Set([
  'ping', 'gpu_info', 'covers', 'game_icons', 'fetch_covers', 'assets_status',
  'dlss5_assets', 'dlss5_choices', 'dlss5_detect', 'get_config', 'gpu_spoof_state',
  'backups_info', 'log_tail', 'list_games', 'list_exes', 'sm_info', 'sm_game_states',
  'check_self_update', 'proxy_advice',
]);

function callSidecar(cmd, args, isRetry) {
  return new Promise(function (resolve, reject) {
    if (!sidecar || sidecarDead) {
      ensureSidecar().then(function () {
        callSidecar(cmd, args, true).then(resolve, reject);
      }, function (e) {
        reject(e);
      });
      return;
    }
    const id = nextId++;
    const timer = setTimeout(function () {
      pending.delete(id);
      reject(new Error('命令超时：' + cmd));
    }, CALL_TIMEOUT_MS);
    pending.set(id, {
      resolve: resolve,
      reject: reject,
      timer: timer,
      cmd: cmd,
      retry: RETRYABLE.has(cmd) ? function () { callSidecar(cmd, args, true).then(resolve, reject); } : null,
    });
    try {
      sidecar.stdin.write(JSON.stringify({ id: id, cmd: cmd, args: args || {} }) + '\n');
    } catch (e) {
      pending.delete(id);
      clearTimeout(timer);
      if (!isRetry) {
        callSidecar(cmd, args, true).then(resolve, reject);
      } else {
        shellLog('写入后台进程失败（' + cmd + '）：' + e.message);
        reject(new Error('后台进程写入失败：' + e.message));
      }
    }
  });
}


function lowerProcessPriority() {
  try {
    const os = require('node:os');
    const pids = [process.pid];
    try {
      for (const m of app.getAppMetrics()) { if (m.pid && pids.indexOf(m.pid) < 0) pids.push(m.pid); }
    } catch (e) {  }
    let ok = 0;
    for (const pid of pids) {
      try { os.setPriority(pid, os.constants.priority.PRIORITY_NORMAL); ok++; } catch (e) {}
    }
    console.log('[prio] 已把 ' + ok + '/' + pids.length + ' 个进程设为低优先级');
    callSidecar('set_efficiency', { pids: pids }).then(function (r) {
      console.log('[eco] 效率模式已设置 ' + ((r && r.ok) || 0) + '/' + ((r && r.total) || 0));
    }).catch(function (e) { console.error('[eco] 设置失败：' + e.message); });
    return ok;
  } catch (e) { return 0; }
}

function dropLegacySidecarName() {
  try {
    const old = path.join(exeDir, 'framegen-sidecar.exe');
    if (fs.existsSync(old) && old !== SIDECAR) {
      fs.rmSync(old, { force: true });
      console.log('[cleanup] 已删除旧版后台进程名 framegen-sidecar.exe');
    }
  } catch (e) {  }
}

function dropLegacyDocs() {
  for (const n of ['使用说明.txt']) {
    try {
      const p = path.join(exeDir, n);
      if (fs.existsSync(p) && fs.statSync(p).isFile()) {
        fs.rmSync(p, { force: true });
        console.log('[cleanup] 已删除旧版遗留的 ' + n);
      }
    } catch (e) {  }
  }
}

function ensureBundledAssets() {
  const src = path.join(exeDir, 'bundled-assets');
  const d5srcEarly = path.join(exeDir, 'bundled-dlss5');
  // 两个目录各自独立：以前缺一个（bundled-assets）就整段返回，
  // 结果 bundled-dlss5 也不会被铺 —— 只是缺了帧生成资产，DLSS5 的更新就白带了。
  if (!fs.existsSync(src) && !fs.existsSync(d5srcEarly)) {
    shellLog('[bundled] 安装目录里没有 bundled-assets / bundled-dlss5：' + exeDir);
    return Promise.resolve({ copied: 0, reason: 'no-bundle' });
  }
  /* 关键代理 version.dll 是「部署」的唯一入口。安装目录里那份被杀毒软件删掉时，
     界面只会显示「资产目录里还没有 version.dll」—— 用户完全看不出原因。
     把源头状态先记进 shell.log，用户报障时一眼就能定位。 */
  if (fs.existsSync(src) && !fs.existsSync(path.join(src, 'version.dll'))) {
    shellLog('[bundled] 警告：安装目录里的 bundled-assets\\version.dll 不存在（安装/解压时被安全软件删了？）');
  }
  const marker = path.join(dataDir, 'bundled-assets.json');
  let prev = null;
  try { prev = JSON.parse(fs.readFileSync(marker, 'utf8')); } catch (e) {  }
  return callSidecar('assets_status').then(function (st) {
    const dst = (st && st.dir) || path.join(dataDir, 'assets');
    try { fs.mkdirSync(dst, { recursive: true }); } catch (e) {}
    let copied = 0;
    const names = [];
    const removedLegacy = [];
    const skipMarker = prev && prev.version === app.getVersion();
    const copyTree = (srcDir, dstDir, relPrefix) => {
      try { fs.mkdirSync(dstDir, { recursive: true }); } catch (e) {}
      for (const f of fs.readdirSync(srcDir)) {
        const s = path.join(srcDir, f);
        const t = path.join(dstDir, f);
        const rel = relPrefix ? relPrefix + '/' + f : f;
        try {
          const st = fs.statSync(s);
          if (st.isDirectory()) {
            copyTree(s, t, rel);
            continue;
          }
          if (!st.isFile()) continue;
          if (skipMarker) {
            let ok = false;
            try { ok = fs.statSync(t).size === st.size; } catch (e) { ok = false; }
            if (ok) continue;
          }
          fs.copyFileSync(s, t);
          copied++; names.push(rel);
        } catch (e) {
          console.error('[bundled] 复制失败', rel, e.message);
          shellLog('[bundled] 复制失败 ' + rel + '：' + e.message);
        }
      }
    };
    // 缺 bundled-assets 时不能调用 copyTree（readdirSync 会抛，整个启动链断掉 → 窗口不出现）
    if (fs.existsSync(src)) copyTree(src, dst, '');

    for (const legacy of ['nvngx_dlssg.dll', 'nvngx_dlss.dll']) {
      try {
        const old = path.join(dst, legacy);
        if (fs.existsSync(old) && fs.statSync(old).isFile()) {
          fs.rmSync(old, { force: true });
          removedLegacy.push(legacy);
        }
      } catch (e) {  }
    }
    const smsrc = path.join(exeDir, 'bundled-sm');
    if (fs.existsSync(smsrc)) {
      const smdst = path.join(dst, 'bundled-sm');
      try { fs.mkdirSync(smdst, { recursive: true }); } catch (e) {}
      // SM 资产现在是**按系列分目录**（rtx30/、rtx20/）—— 必须递归铺开并保留相对路径。
      // 只平铺一层的话，侧车找 rtx30/version.dll 会落空（用户实测报「内置组件不存在」）。
      const walk = function (dir, rel) {
        let entries = [];
        try { entries = fs.readdirSync(dir); } catch (e) { return; }
        for (const f of entries) {
          const s = path.join(dir, f);
          const r = rel ? rel + '/' + f : f;
          let st = null;
          try { st = fs.statSync(s); } catch (e) { continue; }
          if (st.isDirectory()) { walk(s, r); continue; }
          if (!st.isFile()) continue;
          const t = path.join(smdst, r);
          try {
            fs.mkdirSync(path.dirname(t), { recursive: true });
            if (skipMarker) {
              let ok = false;
              try { ok = fs.statSync(t).size === st.size; } catch (e2) { ok = false; }
              if (ok) continue;
            }
            fs.copyFileSync(s, t);
            copied++; names.push('bundled-sm/' + r);
          } catch (e) {
            console.error('[bundled] 复制失败', r, e.message);
            shellLog('[bundled] 复制失败 bundled-sm/' + r + '：' + e.message);
          }
        }
      };
      walk(smsrc, '');
      // 旧版平铺的 version.dll 已被 rtx30/ 子目录取代：清掉，避免新旧两份并存
      try {
        const legacyFlat = path.join(smdst, 'version.dll');
        if (fs.existsSync(legacyFlat) && fs.statSync(legacyFlat).isFile()) {
          fs.rmSync(legacyFlat, { force: true });
          removedLegacy.push('bundled-sm/version.dll(旧平铺)');
        }
      } catch (e) {}
    }

    const d5src = path.join(exeDir, 'bundled-dlss5');
    if (fs.existsSync(d5src)) {
      const d5dst = path.join(dst, 'DLSS5');
      try { fs.mkdirSync(d5dst, { recursive: true }); } catch (e) {}
      // 内置效果包是子目录（effects\Shaders\...、effects\Textures\...）：必须递归铺开
      const walkD5 = function (dir, rel) {
        let entries = [];
        try { entries = fs.readdirSync(dir); } catch (e) { return; }
        for (const f of entries) {
          const s = path.join(dir, f);
          const r = rel ? rel + '/' + f : f;
          let st = null;
          try { st = fs.statSync(s); } catch (e) { continue; }
          if (st.isDirectory()) { walkD5(s, r); continue; }
          if (!st.isFile()) continue;
          const t = path.join(d5dst, r);
          try {
            fs.mkdirSync(path.dirname(t), { recursive: true });
            if (skipMarker) {
              let ok = false;
              try { ok = fs.statSync(t).size === st.size; } catch (e2) { ok = false; }
              if (ok) continue;
            }
            fs.copyFileSync(s, t);
            copied++; names.push('DLSS5/' + r);
          } catch (e) {
            console.error('[bundled] 复制失败', r, e.message);
            shellLog('[bundled] 复制失败 DLSS5/' + r + '：' + e.message);
          }
        }
      };
      walkD5(d5src, '');
      for (const f of []) {
        const s = path.join(d5src, f);
        const t = path.join(d5dst, f);
        try {
          const st = fs.statSync(s);
          if (!st.isFile()) continue;
          if (skipMarker) {
            let ok = false;
            try { ok = fs.statSync(t).size === st.size; } catch (e) { ok = false; }
            if (ok) continue;
          }
          fs.copyFileSync(s, t);
          copied++; names.push('DLSS5/' + f);
        } catch (e) { console.error('[bundled] DLSS5 复制失败', f, e.message); }
      }
    }
    try {
      fs.writeFileSync(marker, JSON.stringify({ version: app.getVersion(), copied: copied, files: names, at: Date.now() }, null, 2), 'utf8');
    } catch (e) {}
    console.log('[bundled] 铺了 ' + copied + ' 个内置文件 -> ' + dst);
    shellLog('[bundled] 铺了 ' + copied + ' 个内置文件 -> ' + dst);
    if (!fs.existsSync(path.join(dst, 'version.dll'))) {
      shellLog('[bundled] 警告：' + path.join(dst, 'version.dll') +
        ' 不存在 —— 界面会提示「资产目录里还没有 version.dll」（多半是被安全软件删了/拦了）');
    }
    if (removedLegacy.length) {
      console.log('[bundled] 已清掉资产目录里的旧运行库：' + removedLegacy.join('、'));
    }
    return { copied: copied, files: names, dir: dst, removedLegacy: removedLegacy };
  }).catch(function (e) {
    console.error('[bundled] 跳过：', e.message);
    shellLog('[bundled] 跳过：' + e.message);
    return { copied: 0, reason: e.message };
  });
}


const UPDATE_FROM = (function () {
  if (!DEV) return null;
  const i = process.argv.indexOf('--fgm-update-from');
  if (i < 0 || !process.argv[i + 1]) return null;
  return process.argv[i + 1].replace(/^"|"$/g, '');
})();
const UPDATE_YES = DEV && process.argv.includes('--fgm-update-yes');
const SIMULATE_UPDATE = DEV && process.argv.includes('--fgm-simulate-update');
// 仅预览「发现新版本」弹窗（不进入下载）：--fgm-simulate-offer
const SIMULATE_OFFER = DEV && process.argv.includes('--fgm-simulate-offer');
const THEME = (function () {
  if (process.argv.includes('--fgm-dark')) return 'dark';
  if (process.argv.includes('--fgm-light')) return 'light';
  return null;
})();
if (THEME) nativeTheme.themeSource = THEME;
function currentTheme() {
  return THEME || (nativeTheme.shouldUseDarkColors ? 'dark' : 'light');
}

const FAKE_GPU = (function () {
  if (!DEV) return '';
  const i = process.argv.indexOf('--fgm-fake-gpu');
  if (i < 0 || !process.argv[i + 1]) return '';
  return process.argv[i + 1].replace(/^"|"$/g, '');
})();

function runInstaller(installer) {
  let child;
  try {
    child = spawn(installer, [], { detached: true, stdio: 'ignore' });
  } catch (e) {
    dialog.showErrorBox(PRODUCT, '启动安装程序失败：' + e.message + '\n\n可以到发布页手动下载。');
    return;
  }
  child.on('error', function (e) {
    dialog.showErrorBox(PRODUCT, '启动安装程序失败：' + e.message + '\n\n可以到发布页手动下载。');
  });
  child.unref();
  console.log('[self-update] 已启动安装程序：' + installer);
  shellLog('自更新：启动安装程序 ' + installer);
  setTimeout(function () { app.quit(); }, 1500);
}

async function offerSelfUpdate(info) {
  const version = info.latest;
  lastUpdateOffer = { latest: version, current: info.current };
  let proceed = UPDATE_YES || SIMULATE_UPDATE || !!(info && info.simulate);
  if (!proceed && !UPDATE_FROM) {
    // 用**应用内自绘弹窗**（原生 MessageBox 与本软件风格不符，长日志还会被截断）：
    // 主进程只把版本与完整日志发给界面，由界面决定是否下载。
    if (mainWin && !mainWin.isDestroyed()) {
      mainWin.webContents.send('fgm:event:selfupdate-offer', {
        latest: version,
        current: info.current,
        notes: info.notes || '',
      });
      return;
    }
    const r = await dialog.showMessageBox(mainWin, {
      type: 'question',
      buttons: ['下载并安装', '稍后'],
      defaultId: 0,
      cancelId: 1,
      title: '发现新版本',
      message: '发现新版本 v' + version + '（当前 v' + info.current + '）。',
      detail: '现在下载并安装吗？\n\n安装过程会关闭本程序，装完会自动重新打开。' +
        (info.notes ? '\n\n—— 更新内容 ——\n' + String(info.notes) : ''),
      noLink: true,
    });
    proceed = r.response === 0;
  }
  if (!proceed) return;

  if (SIMULATE_UPDATE) {
    const send = function (payload) { if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send('fgm:event:selfupdate', payload); };
    send({ phase: 'start' });
    let f = 0;
    const timer = setInterval(function () {
      f += 0.02;
      send({ fraction: Math.min(f, 1), msg: '（模拟）正在下载新版本… ' + Math.round(Math.min(f, 1) * 100) + '%' });
      if (f >= 1) { clearInterval(timer); send({ phase: 'done' }); }
    }, 250);
    return;
  }
  let installer = UPDATE_FROM;
  if (!installer) {
    selfUpdateBusy = true;
    if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send('fgm:event:selfupdate', { phase: 'start' });
    try {
      const r = await callSidecar('self_update_download', { version: version });
      if (r && r.canceled) {
        if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send('fgm:event:selfupdate', { phase: 'canceled' });
        return;
      }
      installer = r && r.installer;
    } catch (e) {
      selfUpdateBusy = false;
      if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send('fgm:event:selfupdate', { phase: 'error' });
      dialog.showErrorBox(PRODUCT, '下载新版本失败：' + e.message +
        '\n\n可以到发布页手动下载安装包。');
      return;
    }
    selfUpdateBusy = false;
    if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send('fgm:event:selfupdate', { phase: 'done' });
  }
  if (!installer) {
    dialog.showErrorBox(PRODUCT, '没有拿到安装包路径，已取消。');
    return;
  }
  runInstaller(installer);
}

const PICKERS = {
  // 用户定的规则：手动添加一律**选 exe**（不是选文件夹）——部署就落在 exe 所在的那一层，
  // 从根上避免"选了上层目录导致部署无效"。所以 add_game 也走 exe 选择 + pick_game_exe_at
  // （sidecar 会取 exe 所在的那一层入库，与 DLSS5 / SM 的手动添加同一份逻辑）。
  // 标题/过滤器名是**中英各一套**：弹窗弹出时从界面读当前语言（原生对话框没有 i18n 体系）。
  add_game: {
    title: {
      zh: '选择游戏实际启动的exe（通常在Win64文件夹下，根据实际情况选择exe！）',
      en: 'Pick the exe the game actually launches (usually under the Win64 folder - choose according to the actual situation!)',
    },
    filterName: { zh: '可执行文件', en: 'Executable' },
    properties: ['openFile'],
    extensions: ['exe'],
    next: 'pick_game_exe_at', key: 'exe',
  },
  pick_game_exe: {
    title: {
      zh: '选择游戏实际启动的exe（通常在Win64文件夹下，根据实际情况选择exe！）',
      en: 'Pick the exe the game actually launches (usually under the Win64 folder - choose according to the actual situation!)',
    },
    filterName: { zh: '可执行文件', en: 'Executable' },
    properties: ['openFile'],
    extensions: ['exe'],
    next: 'pick_game_exe_at', key: 'exe',
  },
  pick_assets_dir: {
    title: { zh: '选择资产目录（存放代理 DLL 与配置）', en: 'Pick the assets folder (holds the proxy DLL and config)' },
    properties: ['openDirectory'],
    next: 'pick_assets_dir_at', key: 'dir',
  },
};

async function pickFileThenCall(spec) {
  // 原生对话框跟着界面语言走：弹出前从渲染进程读一次当前语言（读不到就按中文）
  let lang = 'zh';
  if (mainWin && !mainWin.isDestroyed()) {
    try {
      lang = (await mainWin.webContents.executeJavaScript('window.getLang ? getLang() : "zh"')) || 'zh';
    } catch (e) { lang = 'zh'; }
  }
  const pick = (v) => (v && typeof v === 'object') ? (v[lang] || v.zh) : v;
  const opt = { title: pick(spec.title), properties: spec.properties };
  if (spec.extensions) opt.filters = [{ name: pick(spec.filterName), extensions: spec.extensions }];
  if (mainWin && !mainWin.isDestroyed()) opt.defaultPath = app.getPath('home');
  const r = await dialog.showOpenDialog(mainWin, opt);
  if (!r || r.canceled || !r.filePaths || !r.filePaths.length) return { canceled: true };
  const payload = {};
  payload[spec.key] = r.filePaths[0];
  return callSidecar(spec.next, payload);
}


function createWindow() {
  Menu.setApplicationMenu(null);

  mainWin = new BrowserWindow({
    useContentSize: true,
    width: 1220,
    height: 820,
    minWidth: 960,
    minHeight: 620,
    title: PRODUCT + ' v' + app.getVersion(),
    backgroundColor: '#eef3fb',
    show: false,
    autoHideMenuBar: true,
    icon: path.join(__dirname, 'build', 'icon.ico'),
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      spellcheck: false,
    },
  });

  mainWin.on('page-title-updated', function (e) { e.preventDefault(); });

  mainWin.once('ready-to-show', function () { mainWin.show(); });
  mainWin.webContents.on('did-finish-load', function () {
    if (SIMULATE_OFFER) {
    setTimeout(function () {
      offerSelfUpdate({
        latest: '9.9.9',
        current: app.getVersion(),
        notes: [
          '【更新】',
          '1. DLSS5 插件更换为 GITC 版（原生支持 DX9/10/11/12、Vulkan、OpenGL，32 位/64 位游戏；不自带 DLSS 的游戏也能部署）。',
          '2. 内置 ReShade 效果包（无需联网下载）：标准效果 + SweetFX + iMMERSE 免费套件 + lilium HDR，共 108 个文件。',
          '',
          '【改进】',
          '1. 部署/还原/卸载提示简化：成功只显示【已完成】/【已还原】/【已卸载】，完整明细在“复制”按钮里。',
          '2. 更新窗口改为应用内自绘弹窗，长日志可滚动完整展示。',
          '',
          '【优化】',
          '1. 部署提速：不再备份自己的资产、大文件快速校验、效果包增量铺、安装器哈希缓存。',
          '2. 启动提速：旧版扫描与图标加载延后，游戏库缩略图不再跳变。',
          '',
          '【升级提醒】',
          '1. 已部署旧版 DLSS5（RenoDX）的用户：请先卸载再重新部署。',
          '2. 「彻底卸载」会删除整个 reshade-shaders 目录，自备 fx/预设请先备份。',
          '3. 这一段是故意写长的，用来验证滚动区域是否正常工作——如果你能一路看到这里，说明日志没有被截断。',
        ].join('\n'),
      });
    }, 1500);
  }
  // 仅预览「三类资产有更新」统一弹窗：--fgm-sim-assets[-sm|-dlss5|-dlss5old]
  //   dlss5    = GITC 资产旧了（只告知、重新部署即可）
  //   dlss5old = 旧版 RenoDX 插件（要彻底卸载）
  if (DEV && (process.argv.includes('--fgm-sim-assets') ||
              process.argv.includes('--fgm-sim-assets-sm') ||
              process.argv.includes('--fgm-sim-assets-dlss5') ||
              process.argv.includes('--fgm-sim-assets-dlss5old'))) {
    var simMode = process.argv.includes('--fgm-sim-assets-sm') ? 'sm'
      : (process.argv.includes('--fgm-sim-assets-dlss5old') ? 'dlss5old'
        : (process.argv.includes('--fgm-sim-assets-dlss5') ? 'dlss5' : 'fg'));
    setTimeout(function () {
      if (mainWin && !mainWin.isDestroyed()) {
        mainWin.webContents.send('fgm:event:sim-assets-legacy', { mode: simMode });
      }
    }, 3000);
  }
  // 仅预览下载弹窗（含速度示例）：--fgm-sim-download
  // 走的是**真实进度事件**（与线上同一条渲染路径），只是数值是示例
  if (DEV && process.argv.includes('--fgm-sim-download')) {
    setTimeout(function () {
      if (!mainWin || mainWin.isDestroyed()) return;
      mainWin.webContents.send('fgm:event:selfupdate', { phase: 'start' });
      setTimeout(function () {
        mainWin.webContents.send('fgm:event:selfupdate', {
          msg: '正在下载安装包 62% · 6.2 MB/s',
          fraction: 0.62,
        });
      }, 400);
    }, 2500);
  }
  // （旧的 --fgm-sim-legacy 预览已删除：那条「旧版帧生成」提示与统一清理重复，
  //   要看现在的提示用 --fgm-sim-assets / -sm / -dlss5 / -dlss5old）
  if (SIMULATE_UPDATE) {
    setTimeout(function () {
      // 模拟数据带一段**长日志**：用于预览新弹窗的排版与滚动效果（仅 DEV）
      offerSelfUpdate({
        latest: '9.9.9',
        current: app.getVersion(),
        simulate: true,
        notes: [
          '【更新】',
          '1. DLSS5 插件更换为 GITC 版（原生支持 DX9/10/11/12、Vulkan、OpenGL，32 位/64 位游戏；不自带 DLSS 的游戏也能部署）。',
          '2. 内置 ReShade 效果包（无需联网下载）：标准效果 + SweetFX + iMMERSE 免费套件 + lilium HDR，共 108 个文件。',
          '',
          '【改进】',
          '1. 部署/还原/卸载提示简化：成功只显示【已完成】/【已还原】/【已卸载】，完整明细在“复制”按钮里。',
          '2. 更新窗口改为应用内自绘弹窗，长日志可滚动完整展示。',
          '',
          '【优化】',
          '1. 部署提速：不再备份自己的资产、大文件快速校验、效果包增量铺、安装器哈希缓存。',
          '2. 启动提速：旧版扫描与图标加载延后，游戏库缩略图不再跳变。',
          '',
          '【升级提醒】',
          '1. 已部署旧版 DLSS5（RenoDX）的用户：请先卸载再重新部署。',
          '2. 「彻底卸载」会删除整个 reshade-shaders 目录，自备 fx/预设请先备份。',
          '3. 这一段是故意写长的，用来验证滚动区域是否正常工作——如果你能一路看到这里，说明日志没有被截断。',
        ].join('\n'),
      });
    }, 1500);
  }
    if (blurAllowed()) {
      mainWin.webContents.executeJavaScript("document.body.classList.add('glass')").catch(function () {});
    }
  });
  const loadQuery = { theme: currentTheme() };
  if (FAKE_GPU) loadQuery.fakeGpu = FAKE_GPU;
  mainWin.loadFile(path.join(__dirname, 'ui', 'index.html'), { query: loadQuery });

  mainWin.webContents.setWindowOpenHandler(function (info) {
    shell.openExternal(info.url);
    return { action: 'deny' };
  });

  mainWin.on('closed', function () { mainWin = null; });
}


if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', function () {
    if (mainWin) {
      if (mainWin.isMinimized()) mainWin.restore();
      mainWin.focus();
    }
  });

  app.setAppUserModelId('com.framegen.manager');

  app.whenReady().then(function () {
    ipcMain.handle('fgm:invoke', function (_e, cmd, args) {
      // 自更新下载：界面没带 version 时用最近一次检查到的版本兜底。
      // 再漏就由 sidecar 自己取最新版本 —— 尽量不让用户看到「缺少参数 version」。
      if (cmd === 'self_update_download') {
        args = Object.assign({}, args || {});
        if (!args.version && lastUpdateOffer && lastUpdateOffer.latest) {
          args.version = lastUpdateOffer.latest;
        }
      }
      if (PICKERS[cmd]) return pickFileThenCall(PICKERS[cmd]);
      if (cmd === 'cancel_self_update') {
        // 用户要求「点了停止就**立刻**停」：置位取消标志要等一次阻塞读取返回（最长 20 秒），
        // 所以这里直接**重启后台进程** —— 下载线程随进程消失，立即终止。
        // 正在下载的命令先代答 {canceled:true}（上层按取消走）；进程退出处理会自动拉起新实例，
        // 只读命令也会自动重放，其它功能不受影响。没下完的 .part 有断点续传，下次接着下。
        try {
          for (const [id, q] of Array.from(pending)) {
            if (q.cmd === 'self_update_download') {
              clearTimeout(q.timer);
              pending.delete(id);
              q.resolve({ canceled: true });
            }
          }
          if (sidecar && !sidecarDead) sidecar.kill();
        } catch (e) {  }
        if (mainWin && !mainWin.isDestroyed()) mainWin.webContents.send('fgm:event:selfupdate', { phase: 'canceled' });
        return Promise.resolve({ killed: true });
      }
      if (cmd === 'open_netdisk') {
        return shell.openExternal(NETDISK_URL).then(function () { return { ok: true }; });
      }
      if (cmd === 'open_url') {
        // 只允许停更公告里的三个地址，避免渲染进程变成任意链接跳板。
        const url = String((args && args.url) || '');
        if (!RETIRE_URLS.has(url)) return Promise.resolve({ ok: false, error: 'blocked' });
        return shell.openExternal(url).then(function () { return { ok: true }; })
          .catch(function (e) { return { ok: false, error: String((e && e.message) || e) }; });
      }
      if (cmd === 'open_hags_settings') {
        // 从主进程打开：主进程有前台窗口，设置页才会弹到应用**前面** ——
        // sidecar 是无窗进程，它 ShellExecute 出来的设置页会被压在应用下面（用户反馈）。
        return callSidecar('hags_settings_uri').then(function (r) {
          const uri = (r && r.uri) || 'ms-settings:display-advancedgraphics';
          return shell.openExternal(uri).then(function () { return r; });
        });
      }
      return callSidecar(cmd, args).then(function (result) {
        if (cmd === 'check_self_update' && result && result.hasUpdate) {
          offerSelfUpdate(result);
        }
        return result;
      });
    });
    migrateDataIfNeeded();
    ensureSidecar().catch(function (e) { shellLog('启动后台进程失败：' + e.message); });
    dropLegacyDocs();
    dropLegacySidecarName();
ensureBundledAssets().then(function () {
      createWindow();
      lowerProcessPriority();
      [1000, 3000, 6000, 10000, 20000].forEach(function (ms) { setTimeout(lowerProcessPriority, ms); });
      setInterval(lowerProcessPriority, 30000);
      setTimeout(function () {
        callSidecar('check_self_update').then(function (r) {
          if (r && r.hasUpdate) offerSelfUpdate(r);
        }).catch(function (e) { console.log('[self-update] 启动检查失败（忽略）：' + e.message); });
      }, 6000);
    });
    if (UPDATE_FROM) {
      setTimeout(function () { offerSelfUpdate({ latest: '0.0.0-dev', current: app.getVersion() }); }, 3000);
    }
  });

  nativeTheme.on('updated', function () {
    if (mainWin && !mainWin.isDestroyed()) {
      mainWin.webContents.send('fgm:event:theme', { theme: currentTheme() });
    }
  });

  app.on('window-all-closed', function () { app.quit(); });
  app.on('before-quit', function () {
    quitting = true;
    if (sidecar && !sidecarDead) {
      try { sidecar.stdin.end(); } catch (e) {  }
      try { sidecar.kill(); } catch (e) {  }
    }
  });
}
