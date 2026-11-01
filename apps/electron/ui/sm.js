/* Smooth Motion 页 + 三类资产的旧版清理弹窗（P1-12 第 4 步）：从 app.js 原样搬出，函数名不变。
   依赖其它脚本里的全局：i18n.js 的 t；lib.js 的 `$` / h / badge / backText / errText / fmtSize；
   settings.js 的 optLabelOf / frmLabelOf / presetLabelOf / rememberDeployLevels；
   app.js 的 invoke / toast / busy / showBar / hideBarLater / uiConfirm / paintLibrary /
   refreshLibrary / paintSettings / refreshBackups / doDeploy，以及 CFG / GPU / GAMES 等状态。
   加载顺序：i18n.js -> lib.js -> settings.js -> deploy.js -> dlss5.js -> sm.js -> app.js */
/* 这里原来还有 checkSmLegacy()：单独弹「Smooth Motion 组件有新版本」。
   现在三类资产（帧生成 / SM / DLSS5）合并成一个弹窗（见下面的 checkAssetsLegacy），
   它没有任何调用点了，已随本轮代码结构审计删除（见 docs/CHANGELOG.md）—— 里面的 sm_legacy_scan 命令
   仍然被 checkAssetsLegacy 用着，别一起删。 */

// ---------------------------------------------------------------- 三类资产的旧版清理
//
// 口径（产品已定）：任何一类资产的「已部署文件」和当前内置资产对不上，**启动后直接清干净**，
// 不再先问用户 —— 旧插件配新运行时是对不上的（帧生成 0.4.x 尤其明显），留着只会让人以为
// "还能凑合"。清完弹一个**只有【我知道了】**的告知窗：哪类资产更新了、清了哪些游戏、
// 哪些没清掉。
//
// 每一类的清理命令：
//   · 帧生成：restore（按记录放回原件 / 删掉我们铺的文件）+ clean_legacy（0.3.x 遗留）；
//   · Smooth Motion：sm_set_game(on:false)（先重置驱动 profile）+ sm_asset_restore（取回组件）；
//   · DLSS5：dlss5_uninstall —— **彻底卸载**（官方卸载器 + 插件/模型/残留一起走）。
//     这里原来用的是 dlss5_restore（只收我们的文件、保留 ReShade 本体），实测结果：
//     清完之后游戏目录里 ReShade 本体 / ini / 效果包还在，用户还得再去点一次「卸载」——
//     等于自动清理只做了一半。产品决定：统一按「彻底卸载」处理（用户自备的 reshade-shaders
//     会一并删掉，重新部署时由官方安装器重新铺）。
// 清不掉的（游戏还开着 / 文件被占 / 没权限）不阻断其余游戏，如实列进弹窗让人手动处理。

function legacyKindLabel(kind) {
  if (kind === 'sm') return t('Smooth Motion');
  if (kind === 'dlss5') return t('DLSS5');
  return t('帧生成');
}

// 分组前缀（含冒号）。整条进词表，英文里才是「· Frame generation: 」而不是拼接出的全角冒号。
function legacyKindPrefix(kind) {
  if (kind === 'sm') return t('· Smooth Motion：');
  if (kind === 'dlss5') return t('· DLSS5：');
  return t('· 帧生成：');
}

// 把「命中的游戏名」映射成一条条清理任务（只做映射、不执行 —— 预览与自测都用它）。
function legacyClearJobs(fgNames, smNames, d5Names) {
  var jobs = [];
  var byName = function (n) {
    return GAMES.filter(function (x) { return x.name === n; })[0];
  };
  var exeOf = function (g) { return g.exe ? g.exe : null; };
  (fgNames || []).forEach(function (n) {
    var g = byName(n);
    if (!g) return;
    // 帧生成装在**渲染 EXE 所在目录**（g.target，常常是 …\Binaries\Win64），
    // 不是游戏根目录。以前这里用 g.dir，restore 永远「没有部署记录」，
    // 启动清理对这类游戏等于没做。
    var dir = g.target || g.dir;
    jobs.push({
      kind: 'fg', name: n, cmds: [
        { cmd: 'restore', args: { dir: dir } },
        { cmd: 'clean_legacy', args: { dir: dir } },
      ]
    });
  });
  (smNames || []).forEach(function (n) {
    var g = byName(n);
    if (!g) return;
    jobs.push({
      kind: 'sm', name: n, cmds: [
        // 必须先「关闭」：只调 sm_asset_restore 会留一个"驱动里还开着"的假状态
        { cmd: 'sm_set_game', args: { name: g.name, exe: exeOf(g) || '', on: false, dir: g.dir } },
        { cmd: 'sm_asset_restore', args: { dir: g.dir, exe: exeOf(g) } },
      ]
    });
  });
  (d5Names || []).forEach(function (n) {
    var g = byName(n);
    if (!g) return;
    // 一律彻底卸载：只做还原会留下 ReShade 本体 / 效果包，用户还得再点一次卸载
    jobs.push({ kind: 'dlss5', name: n, cmds: [{ cmd: 'dlss5_uninstall', args: { dir: g.dir, exe: exeOf(g) } }] });
  });
  return jobs;
}

// 顺序执行清理任务：单条失败不中断，逐条记下结果（弹窗要如实写清哪些没清掉）。
function runLegacyClear(jobs) {
  var cleared = [];
  var failed = [];
  var idx = 0;
  var step = function () {
    if (idx >= jobs.length) return Promise.resolve({ cleared: cleared, failed: failed });
    var job = jobs[idx++];
    var n = idx;
    showBar(t('正在清除旧版资产…') + ' (' + n + '/' + jobs.length + ')', n / jobs.length, 'legacy');
    var errs = [];
    var chain = Promise.resolve();
    job.cmds.forEach(function (c) {
      chain = chain
        .then(function () { return invoke(c.cmd, c.args); })
        // 还原可能「部分完成」：文件还在（游戏 / 杀毒软件占着），记录也还在。
        // 这种不能算清理成功，否则告知窗会骗人。
        .then(function (r) {
          if (c.cmd === 'restore' && r && r.partial) {
            errs.push((r.msg && String(r.msg)) || t('还原未完成'));
          }
        })
        .catch(function (e) { errs.push(errText(e)); });
    });
    return chain.then(function () {
      if (errs.length) failed.push({ kind: job.kind, name: job.name, reason: errs[0] });
      else cleared.push({ kind: job.kind, name: job.name });
      return step();
    });
  };
  return step();
}

// 清完之后的告知窗：**只有【我知道了】**（不许点"取消"就把状态留在半路）。
function showClearedDialog(report, kinds) {
  var NL = String.fromCharCode(10);
  var fmt = function (arr) {
    var head = arr.slice(0, 8).join('、');
    if (arr.length <= 8) return head;
    return head + t('…等 ') + arr.length + t(' 个');
  };
  var namesOf = function (list, kind) {
    return list.filter(function (x) { return x.kind === kind; }).map(function (x) { return x.name; });
  };
  var lines = [];
  var failLines = [];
  ['fg', 'sm', 'dlss5'].forEach(function (k) {
    var arr = namesOf(report.cleared, k);
    if (arr.length) lines.push(legacyKindPrefix(k) + fmt(arr));
    var bad = namesOf(report.failed, k);
    if (bad.length) failLines.push(legacyKindPrefix(k) + fmt(bad));
  });
  // `kinds` 只放**这次真的扫到更新**的那几类（由 checkAssetsLegacy 按扫到的列表拼），
  // 这里绝不写死「帧生成、Smooth Motion、DLSS5」——只有 DLSS5 更新时，正文里就只出现 DLSS5。
  var body = t('检测到 {kinds} 资产有更新，已自动清除这些游戏里的旧版部署：', { kinds: kinds.join('、') });
  body += NL + NL + (lines.length ? lines.join(NL) : t('（扫到的旧部署已经干净，没有需要删除的文件）'));
  if (failLines.length) {
    body += NL + NL + t('没能清除的游戏：') + NL + failLines.join(NL) +
      NL + NL + t('请退出游戏后还原/清理后重新部署');
  }
  body += NL + NL + t('请到对应页面重新部署新版本。');
  return uiConfirm({
    title: t('已清除旧版资产'),
    ok: t('我知道了'),
    single: true,
    body: body,
  });
}

// 启动后的统一检查：任何一类资产的已部署文件与内置资产不一致 → 直接清干净 → 告知。
// 按**程序版本**只做一次（`assetsLegacyPrompted`），否则每次启动都扫一遍、删一遍。
function checkAssetsLegacy() {
  if (!CFG) return Promise.resolve();
  // 停更退场版本已经统一清过部署，别再弹旧资产提示。
  if (CFG.retired) return Promise.resolve();
  var ver = CFG.version ? CFG.version : '';
  if (ver.length > 0 && CFG.assetsLegacyPrompted === ver) return Promise.resolve();
  var uniq = function (arr) {
    var out = [];
    arr.forEach(function (n) { if (!n) return; if (out.indexOf(n) < 0) out.push(n); });
    return out;
  };
  /* `fg` 必须在**函数作用域**里声明：它的值在第一个 then 里算出来，第二个 then 还要用。
     （1.6.0 就是把它写进了第一个 then，第二个 then 读不到 → ReferenceError，
      被末尾的 catch 静默吞掉，于是「自动清理」整个没跑，用户只看到另一条旧提示。） */
  var fg = [];
  // 先把库刷出来：清理要靠「游戏名 → 目录」这层映射，库没加载就一个都清不了
  return refreshLibrary().catch(function () { }).then(function () {
    fg = legacyGamesNow().map(function (g) { return g.name; });
    return Promise.all([
      invoke('assets_update_scan').catch(function () { return { fg: [], sm: [], dlss5: [] }; }),
      invoke('sm_legacy_scan').catch(function () { return { games: [] }; }),
      invoke('dlss5_legacy_scan').catch(function () { return { games: [] }; }),
    ]);
  }).then(function (arr) {
    var u = arr[0] ? arr[0] : {};
    // 指纹判定（新）+ 旧布局判定（老记录没有哈希时兜底）
    var fgAll = uniq(fg.concat(u.fg ? u.fg : []));
    var smLegacy = (arr[1] && arr[1].games) ? arr[1].games : [];
    var d5Legacy = (arr[2] && arr[2].games) ? arr[2].games : [];
    var smAll = uniq(smLegacy.concat(u.sm ? u.sm : []));
    /* DLSS5 不再分「旧版 RenoDX」和「GITC 资产旧了」两类 —— 两者都走彻底卸载
       （只还原会留下 ReShade 本体，用户还得手动点一次卸载，见 legacyClearJobs 的说明）。 */
    var d5All = uniq(d5Legacy.concat(u.dlss5 ? u.dlss5 : []));
    if (!fgAll.length && !smAll.length && !d5All.length) return;
    // 先记账再动手：中途崩了也不会每次启动重复清
    invoke('assets_legacy_prompt_done').catch(function () { });
    var kinds = [];
    if (fgAll.length) kinds.push(t('帧生成'));
    if (smAll.length) kinds.push(t('Smooth Motion'));
    if (d5All.length) kinds.push(t('DLSS5'));
    var jobs = legacyClearJobs(fgAll, smAll, d5All);
    return runLegacyClear(jobs).then(function (report) {
      hideBarIfPhase('legacy', 1200);
      return refreshLibrary().catch(function () { }).then(function () {
        return showClearedDialog(report, kinds);
      });
    });
  }).catch(function () { });
}

function setSmText(id, txt) {
  var el = document.getElementById(id);
  if (el) el.textContent = txt;
}
function setChips(host, textKey, gray) {
  if (!host) return;
  host.textContent = '';
  host.appendChild(h('span', { class: 'chip' + (gray ? ' gray' : ''), text: t(textKey) }));
}
function renderSm() {
  if (!document.getElementById('page-sm')) return Promise.resolve();
  var chips = document.getElementById('sm-chips');
  setChips(chips, '检测中…', true);
  setSmText('sm-note', t('正在读取显卡与驱动…'));
  return invoke('sm_info').then(function (r) {
    var allowed = !!(r && r.allowed);
    if (chips) {
      setChips(chips, allowed ? '可以部署' : '当前不适用', !allowed);
    }
    setSmText('sm-gpu', (r && r.gpu) || t('读不到（需要 NVIDIA 显卡）'));
    setSmText('sm-drv', (r && r.driver) || t('读不到'));
    setSmText('sm-min', (r && r.minDriver) || '616.64');
    var gate = document.getElementById('sm-gate');
    if (gate) {
      if (allowed) {
        gate.className = 'gate hidden';
        gate.textContent = '';
      } else {
        gate.className = 'gate';
        gate.textContent = backText(r && r.reason) || t('当前不适用');
      }
    }
    setSmText('sm-note', allowed
      ? t('驱动满足本工具要求（>=') + ((r && r.minDriver) || '616.64') + t('）。')
      : t('不满足要求时无法部署；原因见上面的提示。'));
    var sub = document.getElementById('sm-sub');
    if (sub) sub.textContent = GAMES.length + t(' 个游戏 · 一个游戏只能启用一个引擎（帧生成 / DLSS5 / Smooth Motion）');
    return paintSmList();
  }).catch(function (e) {
    setSmText('sm-note', t('检测失败：') + errText(e));
    setChips(chips, '检测失败', true);
  });
}
function paintSmList() {
  var box = document.getElementById('sm-list');
  if (!box) return Promise.resolve();
  if (!GAMES.length) {
    box.textContent = '';
    box.appendChild(h('div', { class: 'empty', text: t('游戏库是空的 —— 点右上「扫描游戏库」。扫描只读启动器记录，不改动任何游戏文件。') }));
    return Promise.resolve();
  }
  return invoke('sm_game_states', {
    names: GAMES.map(function (g) { return g.name || ''; }),
    exes: GAMES.map(function (g) { return g.targetExe || g.target || ''; }),
  }).then(function (d) {
    var map = {};
    ((d && d.states) || []).forEach(function (s) { map[s.name] = s; });
    var frag = document.createDocumentFragment();
    GAMES.forEach(function (g, i) {
      var st = map[g.name] || {};
      var on = st.value === 1;
      var prof = st.profile
        ? t('驱动配置：') + st.profile + (st.value === null || st.value === undefined ? t('（未设置，跟随全局）') : t('（Smooth Motion = ') + (on ? t('开') : t('关')) + t('）'))
        : t('驱动里还没有这个游戏的配置');
      frag.appendChild(gameCard(g, i, {
        label: on ? t('关闭 Smooth Motion') : t('开启 Smooth Motion'),
        danger: on,
        note: prof,
        onClick: function () { toggleSm(g, !on); },
      }));
    });
    box.textContent = '';
    box.appendChild(frag);
  }).catch(function (e) {
    box.textContent = '';
    box.appendChild(h('div', { class: 'empty', text: t('读取 Smooth Motion 状态失败：') + errText(e) }));
  });
}
function toggleSm(g, on) {
  if (!g || !g.name) return;
  var exe = g.targetExe || '';
  if (!exe) {
    toast(t('这个游戏没有识别到启动用的 EXE，先到「帧生成」页确认游戏路径。'), 'warn', 12000);
    return;
  }
  var deployedFg = deployKind(g.deployed) === 'deployed';
  var manualFg = deployKind(g.deployed) === 'manual';
  if (on && (deployedFg || manualFg)) {
    // 帧生成占着 version.dll 时，解锁组件会把它顶掉 —— 这种情况**直接挡住，不让开**：
    // 两个都想用的话，得先开 Smooth Motion、再去帧生成页换一个别的入口（用户定的规则）。
    var fgProxy = (g.fgAssets && g.fgAssets.proxy) || '';
    if (fgProxy === 'version.dll') {
      uiConfirm({
        title: t('这个游戏已经装了帧生成'),
        ok: t('我知道了'),
        single: true,
        danger: true,
        body: t('帧生成DLSS-FG已部署！请勿再开启Smooth Motion！以免导致DLSS-FG失效！若想尝试两个都开启，请先开启Smooth Motion再去帧生成页面部署'),
      });
      return;
    }
    uiConfirm({
      title: t('这个游戏已经装了帧生成'),
      ok: t('仍然开启'),
      body: t('该游戏疑似已经部署了帧生成！不建议继续开启SM！但你可以忽略这条提示继续开启'),
    }).then(function (yes) {
      if (yes) doSmWrite(g, exe, on);
    });
    return;
  }
  if (on && deployKind(g.deployed) === 'occupied') {
    uiConfirm({
      title: t('这个入口文件不是本工具放的'),
      body: t('这个游戏目录里已经有一个不是本工具放的入口文件（') + g.deployed + t('）。\n\n继续会先把它原样备份，再放入 Smooth Motion 的解锁组件；之后可以用关闭或还原取回。\n\n要继续吗？'),
    }).then(function (yes) {
      if (yes) doSmWrite(g, exe, on);
    });
    return;
  }
  if (on && GPU && GPU.routeKey === 'sm75') {
    // RTX 20 支持是本轮新增：上游清单标注「未经实机验证」，部署前如实说明并让用户确认
    uiConfirm({
      title: t('开启 Smooth Motion（RTX 20）'),
      ok: t('仍然开启'),
      body: t('RTX 20 的 Smooth Motion 支持是实验性的：上游标注该运行库未经实机验证。确定继续吗？'),
    }).then(function (yes) {
      if (yes) doSmWrite(g, exe, on);
    });
    return;
  }
  doSmWrite(g, exe, on);
}
function doSmWrite(g, exe, on) {
  var dir = g.target || g.dir || '';
  var first = on
    ? invoke('sm_asset_deploy', { dir: dir })
    : Promise.resolve(null);
  return first.then(function (a) {
    return invoke('sm_set_game', { name: g.name, dir: dir, exe: exe, on: !!on }).then(function (r) {
      return { r: r, asset: a };
    });
  }).then(function (o) {
    var r = o.r;
    var extra = on ? backText((o.asset && o.asset.message) || '') : '';
    var tail = !on
      ? invoke('sm_turn_off_game', { name: g.name, dir: dir, exe: exe })
          .then(function (m) { return backText(m && m.message) || t('已关闭'); },
                function (e) { return { text: t('驱动设置关闭失败：') + errText(e), bad: true }; })
          .then(function (off) {
            return invoke('sm_asset_restore', { dir: dir, exe: exe }).then(function (a) {
              var offText = typeof off === 'string' ? off : off.text;
              var offBad = typeof off === 'string' ? false : !!off.bad;
              return { text: [offText, a && a.message].filter(Boolean).join(t('；')), bad: offBad };
            }, function (e) {
              var offText = typeof off === 'string' ? off : off.text;
              var offBad = typeof off === 'string' ? false : !!off.bad;
              return { text: [offText, t('解锁组件没取回：') + errText(e)].filter(Boolean).join(t('；')), bad: true };
            });
          })
      : Promise.resolve({ text: extra, bad: false });
    return tail.then(function (res) {
      hideBarLater(1200);
      var tailText = (res && res.text) || '';
      var tailBad = !!(res && res.bad);
      var ok = r && r.ok && !tailBad;
      // 用户定的口径：成功只说【已开启】/【已关闭】，长说明一律进日志（core 侧已记录）；
      // 但失败的部分（驱动没写成 / 组件没取回）必须照说，不然没法排障。
      var msg = on ? t('已开启') : t('已关闭');
      if (!ok || tailBad) msg += (tailText ? t('；') + tailText : '');
      toast(msg, ok ? 'ok' : 'warn', ok ? 9000 : 14000);
      // 开关成功后重绘列表（按钮文案/驱动配置行都跟着变）。
      // 驱动侧状态生效偶尔有延迟：800ms 后再兜底重绘一次，避免按钮停在旧状态（用户反馈）。
      var refresh = function () { return paintSmList(); };
      refreshLibrary().catch(function () { }).then(refresh).catch(function () { });
      setTimeout(refresh, 800);
      return r;
    });
  }).catch(function (e) {
    hideBarLater(1200);
    toast(t('Smooth Motion 操作中止：') + errText(e), 'err', 10000);
  });
}
