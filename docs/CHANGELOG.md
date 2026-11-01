# 变更记录

## v1.7.0

**停止维护公告**

- 本项目停止更新维护，请转移至新项目 **FrameGen Manager Next**：<https://github.com/XiaoQAQ10086/FrameGen-Manager-Next>。
- 安装 1.7.0 后首次启动会自动清除**所有已部署游戏**里的帧生成、Smooth Motion 与 DLSS5 文件，并删除相关备份记录；清理完成后弹窗提供「打开新项目 Github 仓库」「Github 下载」「去网盘下载」三个入口。
- 停更后禁用部署、清理、显卡伪装与软件更新入口，避免继续在游戏目录里写文件。
- 清理会逐个目录复查：文件被游戏或安全软件占用时不会谎报成功，会列出未清干净的项目并提示关闭游戏后重启重试；备份记录删不掉时同样保留记录、下次启动继续清。
- 旧配置缺少退场字段时按默认值读取，不会把用户原有设置重置。

## v1.6.12

**修复**

- 自更新下载 `self_update_download` 的 `version` 改为**可选**：界面漏传时主进程用最近一次检查到的版本兜底，后台再兜底取最新发布版本；从 1.6.12 起不会再出现「缺少参数 version」。
- **注意**：1.6.7 及更早版本的自动更新按钮本身有 bug，必须手动安装 1.6.8+；本次修复只能保证 1.6.12 及之后的更新路径。
- 保留 1.6.11 的 2077 DLSS 误删修复。

## v1.6.11

**修复**

- 还原 / 启动自动清理不再误删游戏自带的 `nvngx_dlssg.dll` / `nvngx_dlss.dll`。Cyberpunk 2077 的 `bin\x64` 里这两份 NVIDIA DLL 与旧版内置资产**逐字节相同**，旧逻辑按「名字 + 大小 + 哈希」当成 0.3.x 残留删掉，结果游戏里的 DLSS 帧生成选项直接消失，只能靠 Steam「验证文件完整性」恢复。
  - `legacy_leftover_candidates` / `clean_legacy_leftovers` 不再把这两份 NVIDIA 运行库列为清理候选；
  - `manual_files`（无部署记录时的手动清理）不再删除它们；
  - 还原时有备份的游戏原件会正常写回；**没有备份的保留**并提示，不再「删了却补不回来」；
  - 部署时不再把游戏自带的这两份文件误判成「我们的文件」而不备份。
- 新增回归测试：受保护 DLL 出现在游戏目录时，清理候选为空、清理不删除、`is_our_artifact` 不认领。

## v1.6.10

**改进**

- 手动添加游戏时，选中的 exe 读不了会给出明确原因：微软商店 / Xbox 的 `WindowsApps` / `XboxGames` 受保护目录、普通目录的 ACL / 管理员权限、游戏或杀软占用；不再只显示 Windows 的原始「没有打开的权限」。
- 修 `add_game` 的 rfd 兜底路径：之前选完 exe 只把目录传进入库逻辑，选中的 exe 被丢掉了；现在一并传入。

## v1.6.9

**改进**

- 日志：每条 IPC 命令记录 `cmd / args / 耗时 / ok / 错误码` 到 `events.jsonl`；`framegen.log` 与 `events.jsonl` 超过 4 MB 自动滚到 `*.prev`。日志页新增「导出诊断包」：把当前/上一份日志、事件、脱敏后的配置和版本信息打成 zip（不含游戏库 / 存档）。
- 新增 **IPC 参数契约检查**（`packaging/ipc-contract.json` + `ipc-contract-check.js`）：打包前静态核对界面 `invoke` 调用是否漏传必需参数，防止再出现 `self_update_download` 少 `version` 这类问题；已接进 preflight 和 build-release 的七道校验。
- 日志页只保留 **「导出日志信息」**（原来的「刷新」「复制诊断信息」已删除）；自动刷新改为 10 秒一次、内容不变不碰 DOM；`log_tail` / `ping` 不再写命令日志，空闲时日志不会自己增长。

## v1.6.8

**修复**

- 修复点击「下载并安装」提示「下载更新失败：缺少参数 version」：界面调用 `self_update_download` 时漏传 `version`，后台在开始下载前就拒绝了（1.6.7 用户实测）。
- 修正错误码误报：`缺少参数` 以前被 `AST-004`（内置资产缺失）的「缺少」关键词吞掉，现在 AST-004 只匹配资产类文案，新增 `REQ-017`（请求缺少参数）。

**更新**

- 顶部 Smooth Motion / DLSS5 去掉「（实验）」标记。
- 新增顶部「日志」页：环境信息（版本 / 系统 / 显卡 / 驱动 / 游戏库 / 日志目录）、运行日志（最近 300 行，可刷新）、「复制诊断信息」（最近 2000 行 + 主进程 shell.log）和「打开日志目录」。日志页只在可见时轮询，用户往上翻时不会被自动拽回底部。

## v1.6.7

**更新**

- 更新内置的 DLSS5 资产文件 GITC Uplift 1.7.0
- 修复上一版本 1.6.3 部署帧生成点击还原后还需要再清理一遍的 BUG

**移除**

- 移除三项检测（用户要求）：**反作弊**、**是否自带 DLSS（Streamline）**、**游戏引擎**。
  - 游戏库卡片 / 详情框不再显示「内核级反作弊」「自带 DLSS / 未检出 DLSS」「引擎：…」；
  - 部署 / DLSS5 对话框不再显示「游戏引擎」「是否自带DLSS」「反作弊」三行，也不再弹「检出内核级反作弊」的风险提示；
  - DLSS5 页不再按「自带 DLSS」分两栏，合并为一个「游戏」列表；
  - core 不再扫描反作弊服务/驱动、引擎特征、Streamline 痕迹，扫描更快；缓存里也不再写这三个字段。
- 图形 API 检测保留（DLSS5 部署 / 代理选择要用）。

## v1.6.6

**改进**

- 资产缺失提示改成产品给的短文案：`内置资产缺失：<缺的文件>` + `可能的原因：杀毒软件 / Windows Defender 拦截了这些文件`（不再在弹窗里铺安装目录 / 资产目录两行路径）
- **卸载器补全（手动卸载）**：以前只删 `%APPDATA%\FrameGen-Manager` 和安装目录，实测还会留下 Electron 的 userData（`%APPDATA%\FrameGen Manager`：语言 / localStorage / 缓存）、老版 electron-updater 的下载缓存（`%LOCALAPPDATA%\framegen-manager-updater`，本机实测 376 MB）、老 Tauri 的 WebView2 数据（`%LOCALAPPDATA%\com.framegen.manager`）、运行时 bundle 缓存（`%LOCALAPPDATA%\DlssgSm86`）与更老的 `%APPDATA%\DLSSG-Manager`。现在：
  - 先 `taskkill` 主程序 / 后台进程（不结束的话 `$INSTDIR` 里的 exe 被占用，安装目录会删不干净）；
  - 再把上面这些目录连同 `%TEMP%\fgm-selfupdate` 一起清掉；
  - 注册表清 `Software\<APP_GUID>`（HKCU / HKLM / WOW6432Node）与老 Inno 卸载键（HKCU / HKLM / WOW6432Node）；
  - 检测到显卡名称伪装备份时弹窗提示先还原（`DeviceDesc` 在 HKLM，卸载器不静默改注册表）；
  - 静默卸载（升级）仍然保留所有用户数据，不能把升级做成清库。

## v1.6.5

**修复**

- 部署时提示「资产目录里还没有 version.dll」，卸载重装也一样（用户反馈）。**不是卸载残留**：手动卸载会删 `%APPDATA%\FrameGen-Manager` 与安装目录，就算有残留，启动铺资产时也会按大小补齐缺失文件。真正原因是**内置代理 DLL 被安全软件删了 / 拦了** —— `version.dll` 从 0.4.1 起没有数字签名，Defender / 360 / 火绒常把它当注入工具；安装目录 `bundled-assets` 与 `%APPDATA%\FrameGen-Manager\assets` 各一份，两份都没了就无处可补。现在：
  - 主进程把铺资产的每个失败（源文件缺失、复制失败、关键文件 `version.dll` 不落盘）写进 `logs\shell.log`，不再只打控制台；
  - 界面发现资产缺文件会先调新增的 `assets_repair`（从安装目录的三套内置资产 `bundled-assets` / `bundled-sm` / `bundled-dlss5` 只补缺失/大小不对的文件，不覆盖用户资产）；补回来就提示「已从安装目录补回 N 个」；**即使补回了一部分、只要还有缺的文件，告知窗也照常弹出**（以前会只弹 toast，把「还缺 version.dll」这条关键提示吞掉）；
  - 补不回来时弹窗写明**缺哪个文件**、**安装目录 / 资产目录两个路径**，并提示多半被杀毒软件删了、加白名单后重装；
  - 安装器在非静默安装完成后检查 `bundled-assets\version.dll` 与 `bundled-sm\rtx30\version.dll`，没落盘就当面提示（静默更新不弹，避免卡住自动更新）；
  - 反馈日志（「复制诊断信息」）除 sidecar 的 framegen.log 外，还会附上主进程的 `shell.log` —— 资产铺设失败的线索都在那里。
- DLSS5 部署前先检查游戏目录是否可写：不可写时官方 ReShade 安装器只会抛一句乱码的「Failed to install dxgi.dll: 对路径…的访问被拒绝」。现在直接提示「以管理员身份运行 / 给游戏目录加白名单 / 先完全退出游戏」

## v1.6.4

**修复**

- 还原没做完却报「已还原」，还顺手把部署记录删了 —— 之后界面显示「本工具无记录」，用户只能再点一次「清理」，而「清理」没有原件备份，游戏原件再也放不回来（1.6.3 用户实测）。根因：游戏 / 启动器 / 反作弊还开着（或杀毒软件正在扫）时 `version.dll` 被独占，`backup_restore::remove_created` 删不掉只记不报，`restore_with` 仍按成功收尾并 `remove_dir_all` 备份目录。现在：删文件带重试（5 次、退避合计约 2 s，含去掉只读位）；删不掉的逐个列进结果，**备份与部署记录保留**，界面显示「还原未完成」并把原因摊在弹窗里 —— 完全退出游戏后再点一次「还原」就能收尾
- 还原时清单里的文件读不出来（被独占）也会被静默跳过：同一处现在记为「没法确认归属」，同样保留记录并如实报告
- 还原重试时，上一次已经放回原位的文件会被当成「内容被别人改过」跳掉，导致记录永远删不掉：现在先拿备份里的原件字节比一次，已经回原样就跳过（不再误判）
- 启动时的旧版资产自动清理，帧生成用错了目录（`g.dir` 游戏根目录，而不是 `g.target` 渲染 EXE 所在目录）：对霍格沃茨这类 `…\Binaries\Win64` 的游戏，`restore` 永远「没有部署记录」，等于没清。现在按部署时用的目录清，且部分还原会算作「没清掉」列进告知窗
- `restore` 命令改为返回 `{msg, partial, leftover}`：界面不再把「部分还原」显示成「已还原 / 未部署」

## v1.6.3

**修复**

- 启动清理时底部进度条会永远停在「正在删除插件 / 模型与运行时残留…」：DLSS5 卸载发出的进度事件不带「相位」，把进度条上记着的相位（`legacy`）覆盖成了空，收条逻辑按相位比对就对不上了。现在进度条会记住最近一次的相位，事件只更新文字与百分比；条收掉时相位一并清空

## v1.6.2

**改进**

- 资产更新的自动清理，对 DLSS5 改为**彻底卸载**（官方卸载器 + 插件 / 模型 / 残留一起走）：1.6.1 只做「还原」，游戏目录里会留下 ReShade 本体与效果包，用户还得再手动点一次「卸载」才算干净。代价：用户自备的 `reshade-shaders`（fx / 预设）会被一并删除，重新部署时由官方安装器重新铺
- 「彻底卸载」现在会把 ReShade 给 DX12 游戏铺的 `D3D12\D3D12Core.dll` 也收走（官方卸载器不管这个目录）。只在「`D3D12\` 里仅此一个文件、且版本资源写着 Microsoft」时才删，避免误删游戏自带的 Agility SDK

## v1.6.1

**修复**

- 启动清理其实一直没跑：`checkAssetsLegacy` 里的 `var fg` 写进了第一个 `then`，第二个 `then` 读不到 → `ReferenceError` 被末尾的 `catch` 静默吞掉。1.6.0 实测表现为「没清理、也没弹新告知窗」。现在 `fg` 提到函数作用域，并加 e2e 断言钉住（调用 `checkAssetsLegacy` 不许抛异常）
- **删掉两条重复的旧提示**（它们比统一清理先跑，用户看到的是最旧那条）：`boot` 里的 `framegen_cleanup_outdated` 自查（帧生成专用、弹「旧版帧生成已清除」）与 `deploy.js` 的 `maybePromptLegacyUpgrade`（弹「是否重新部署新版本文件」确认框）。1.6.0 实测：升级后提示的是「帧生成」并清了霍格沃茨，而真正要更新的 DLSS5（Nomad Drive Demo）一个文件都没动，就是被这两条抢先了
- `dlss5::is_outdated` 认不出 **64 位插件**变了：它拿前缀 `gitc-uplift` 去 `asset_dir` 里拼路径（实际文件名是 `gitc-uplift.addon64`），永远 `continue` 掉。只换 addon64 的那种更新会完全漏报；现在直接问 `find_addon()` 要真实路径
- 安装器底色：去掉 `MUI_BGCOLOR` / `MUI_TEXTCOLOR` 的自定义（electron-builder 的模板里文字色没覆盖到所有控件，结果黑底黑字看不见），回到系统默认浅色页面；品牌靠侧栏 / 顶栏位图

## v1.6.0

**新增**

- 安装器 / 卸载器品牌化：真实应用图标、深色完成页、中文侧栏、顶栏绿线（NSIS 位图）

**改进**

- P1：删除 0 引用常量；新增 `assets_audit` 统一判定入口；错误响应带稳定错误码（`DRV-001` 等）；`bump-version.ps1` 一键升版
- P2：卸载时清理数据目录 / `%LOCALAPPDATA%` / 自更新暂存；**不动游戏目录里已部署的文件**
- 安全：清单路径走 `safe_join`（拒绝 `..` / 绝对路径 / UNC / 保留名）；sidecar 写目录命令统一校验目标
- 体验：驱动低于 R580 时部署弹窗直接拦截并说明；旧版帧生成被自动清理后改为**弹窗**告知（不再只发 15 秒 toast）
- 工程：本地 git 仓库 + 源码 `.gitignore`；新增 `docs/ARCH.md`

- 超分：部署弹窗的「超分加速」改为**超分预设五选一**（`Off` / `M-INT8` / `L-INT8` / `M-FP16` / `L-FP16`），标签同步改名；批量重新部署不再丢预设；老的 `0/1` 选择自动迁移
- 驱动：部署前检测 **R580** 门槛（sidecar 直接给主版本号），低于则拦截并说明「部署成功但游戏里没效果」；读不到版本时只提示不误拦
- 兼容：修「代理文件已被覆盖或丢失」——0.4.1 代理换了签名，改为**按内容哈希**认归属；ReShade 的 `dxgi.dll` 不再被误判为帧生成占用（改为读 PE 版本串 + 已知哈希兜底）
- 诊断：日志面板新增【诊断】按钮，一键复制「版本 / 驱动 / 语言 / 日志尾」
- 工程：`packaging/preflight.ps1`（双 CR / 语法 / 词表 / 单测一键自检）；代理表与真实资产的一致性单测；`safe_join` 去掉不安全回退

- 工程（本轮补齐）：前端公共层拆到 `ui/lib.js`（DOM 工具/回译/格式化/复制/部署选项记忆），`app.js` 2700 → 2596 行；词表检查同步覆盖 `lib.js`
- 升级策略（产品决定）：**任何一类资产（帧生成 / Smooth Motion / DLSS5）有更新，启动后直接清干净已部署的旧版本，不再先问**；清完弹一个**只有【我知道了】**的告知窗，写清「哪类资产更新了（只写真的更新的那几类）/ 清了哪些游戏 / 哪些没能清除 + 下一步怎么办」。每类都走「把我们的部署收回去」的命令：帧生成 `restore` + `clean_legacy`；SM `sm_set_game(on:false)` + `sm_asset_restore`；DLSS5（GITC）`dlss5_restore`（只收我们的文件，**不动用户自备的 reshade-shaders**）；DLSS5（旧版 RenoDX）`dlss5_uninstall`（布局不兼容，必须彻底卸载）。清不掉的不阻断其余游戏，逐条列进弹窗；按程序版本只做一次
- 修复：内置资产铺设不再因为缺 `bundled-assets` 就整段跳过 `bundled-dlss5`（两个目录各自判断），并给 `copyTree` 补存在性保护（否则 `readdirSync` 抛异常会中断启动链，窗口都不出来）
- 资产：DLSS5 插件升级到 **GITC Uplift 1.7.0**（上游 2026-10-27 发布）：`gitc-uplift.addon64` / `gitc-uplift.addon32` / `gitc-uplift-helper64.exe` 三个二进制换新，`Uplift.fx` / `UpliftMask.fx` 与神经网络模型（310.9.16）**未变**；文件名与目录布局不变，`dlss5.rs` 里的常量无需改动。SHA-256 与发布包 `SHA256SUMS.txt` 逐条一致（zip 本体也核过），`THIRD_PARTY_NOTICES.txt` 同步更正 —— 那一节原来还写着 RenoDX v8.5 插件与 310.8 模型，和实际内置的 GITC 1.7.0 / 310.9.16 对不上
- 工程（代码结构审计）：死代码清理（Rust `sm::orphan_profiles` / `recorded_profile_names`、`BackupPlan::created/summary`；前端 `checkSmLegacy` / `checkDlss5Legacy` —— 两类资产合并成一个清理弹窗后就没有调用点了）；**原子写 JSON 收口**到 `util::write_json_atomic` / `write_text_atomic`，配置 / 游戏库缓存 / GPU 名称备份 / SM 记录 / 测速记忆从"直接 fs::write"改成原子写（写一半崩溃会让下次启动读不出来，游戏库那份甚至会按空库处理）；core 的 `windows-sys` 去掉 4 个已无代码可用的 feature（Crypto/WinTrust/Security/Memory，都是 egui 版字体映射与 WinVerifyTrust 的遗留）；`cargo clippy --all-targets -- -D warnings` 在 core 与 sidecar **双双回到 0 告警**（原来 13 处：过时文档注释、可合并 if、过多参数、多余借用、bool 断言等），CI 那道门禁终于真的能过
- 工程（P1-15 折中版）：错误码成为归类**唯一来源**（`crates/core/src/err.rs`，17 个编号；sidecar 不再自己维护一份 `err_code`，补齐了 SM / DLSS5 / 备份 / 扫描 / 配置 / 文件 IO / 系统目录拒绝七类此前一律落到 `GEN-999` 的错误）；英文界面先按中文原文查表，整句没译文时**按编号兜底**（`ui/lib.js` 的 `backText`），编号始终留在消息末尾供报障；`i18n-coverage.py` 新增门禁「core 的每个错误码必须有英文」，preflight 打印 `ERR-CODE` 行
- 修复：`main.js` 拼在错误末尾的 `[CODE]` 会让原文永远匹配不到词表（键是纯中文原文），等于把译文憋掉 —— 现在翻译前先切掉编号，译完再接回
- 工程（P1-12）：前端按页拆文件 —— `app.js` 2596 → 594 行，新增 `ui/settings.js`（设置/关于/更新弹窗/日志）、`ui/deploy.js`（详情对话框 + 部署/还原/清理）、`ui/dlss5.js`、`ui/sm.js`、`ui/library.js`（库页 + 封面/瓦片墙/扫描）；函数名与函数体一律不变（只移动不重写），脚本顺序改为 `i18n → lib → settings → deploy → dlss5 → sm → library → app`，e2e 与词表检查改为从 `index.html` 读脚本清单，`preflight` 的语法门禁覆盖整个 `ui/`
- 工程（P1-7）：备份/还原的公共层抽到 `crates/core/src/backup_restore.rs`（`BackupPlan` + `capture` / `snapshot_one` / `load_originals` / `apply` / `rollback`），帧生成、解锁组件、DLSS5 三处不再各写一份；所有清单路径统一 `safe_join`，备份名统一扁平化，失败回滚如实报告成败（不谎报「已回滚」）
- 工程：新增 `packaging/push-release.ps1`（从**本地仓库**推 main + tag 并校验远端 SHA）与 `packaging/bench.ps1`（启动/扫描/部署基线：54 ms / 49 ms / 524 ms）
- 修复：日志时间戳改用本地时间（`GetLocalTime`）；移除与既有【复制】重复的【诊断】按钮

## v1.5.0

- 帧生成资产升级到 **0.4.1**（内核 / 后端 / xefg / sr / INI 全换，代理与 INI 同步）
- DLSS-G 预设改为 A / B / C（`Auto` 删除，默认 A）；新增 3X
- XeSS 引擎支持 2X / 3X / 4X（选 XeSS 时隐藏 6X，后端夹到 4X）
- 超分加速改为五选一预设（Off / M-INT8 / L-INT8 / M-FP16 / L-FP16）
- 移除帧生成页「帧生成Mod及帧生成运行库」卡片

## v1.4.0

- DLSS5 更换为 GITC Uplift 1.6.2（DX9–12 / Vulkan / OpenGL，32 / 64 位）
- 内置 ReShade 效果包（标准 + SweetFX + iMMERSE + lilium，108 个文件）
