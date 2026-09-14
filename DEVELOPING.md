# 开发说明

面向改代码的人。用户只需要看 [README](README.md)。

## 技术栈与硬约束

Rust + egui/eframe。**不许用** Electron / Tauri / WebView。
目标：安装包 < 10 MB、启动 < 1 s、关闭即退出（不驻留、不轮询、不自动全盘扫描）。

## 构建

    cargo run --release        # 直接跑
    cargo build --release      # 产物 target\release\framegen-manager.exe

### 依赖上的四个坑（都已在 Cargo.toml / .cargo 里规避）

1. **eframe 默认渲染后端是 wgpu**，会拉进 wgpu-core / naga / ash，体积巨大。
   必须 default-features = false，只开 glow + default_fonts。
2. **reqwest 默认 TLS 会拉 aws-lc-sys**，那个 crate 需要 CMake + NASM 才能编译。
   改用 native-tls（Windows 走 schannel），零 C 依赖。
3. **flate2 用来解压运行库的 zip**。它本来就在依赖树里（png -> image -> eframe），
   所以加为直接依赖是零新增下载、零体积增加。
4. **.cargo/config.toml 开了 +crt-static**，去掉对 vcruntime140.dll 的依赖，
   用户不装 VC++ 运行库也能直接跑。实测依赖从 24 个降到 17 个系统 DLL。

## 自测命令

    cargo run -- --selftest        # 扫描游戏库 + 反作弊 + 更新检查 + 入口推荐 + 显卡
    cargo run -- --deploytest      # 部署 / 备份 / 还原 / 冲突 / 已装过本项目 端到端断言
    cargo run -- --canceltest      # 取消下载 + 残留清理
    cargo run -- --downloadtest    # HEAD 取 ETag -> 下载 581B 的 ini -> 校验指纹（0 次 API）
    cargo run -- --speedtest       # 走生产路径（镜像优先 + 签名校验）实测下载速度
    cargo run -- --selfupdate      # 软件自身版本检查 + 版本号比较的边界用例
    cargo run -- --sourcetest      # 选源排序 + 看门狗（本地起慢服务器，不依赖外网）
    cargo run -- --speedall        # 实测每个候选源的速度，打印出下载时的尝试顺序
    cargo run -- --backuptest      # 实测备用源，并验证镜像也会透传 ETag
    cargo run -- --dlssrun         # 下载 DLSS 运行库：直链 -> 下载 -> 解压 -> 删包 -> 验签
    cargo run -- --ziptest <zip> <输出>   # 单独测 zip 解压
    cargo run -- --idtest <文件>          # 看一个文件的签名身份
    cargo run -- --icontest <exe>         # 提取图标并打印尺寸/透明度统计
    cargo run -- --pedump <exe>           # 打印 PE 导入表
    cargo run -- --gpuinfo                # 只读：显卡注册表实例 + 驱动版本 + 备份状态

另有三个只给程序内部用的模式（主程序用 ShellExecuteW("runas") 拉起的提权子进程）：

    --gpuspoof-apply <型号> <结果文件>
    --gpuspoof-restore driver|backup <结果文件>

它们把 JSON 结果写进结果文件后立刻退出，不创建窗口。手工调用等于自己给自己提权，
一般用不上；`--gpuinfo` 已经能看全部状态。

## 调试开关（环境变量）

正常启动不受影响，只在排查 / 截图时用：

| 变量 | 作用 |
|---|---|
| `DLSSG_AUTOSCAN=1` | 启动就扫一次游戏库 |
| `DLSSG_SPOOF_OPEN=1` | 显卡名称伪装卡片默认展开 |
| `DLSSG_NO_CJK_FONT=1` | 不加载中文字体，用来量化字体占多少内存 |
| `DLSSG_FAKE_NEWVER=0.9.9` | 假装远端有新版本，验证标题栏那个下载入口（本地远端同版本时看不到） |
| `DLSSG_AUTOSPEED=1` | 启动就跑一次测速，省得截图脚本去点按钮 |

界面自测只能靠截图像素分析（本机没有可自动化的 GUI 断言框架），所以这些开关很关键。
**注意**：分析截图时要按窗口标题 FrameGen Manager 找窗口 —— debug 版是控制台程序，
会额外有一个标题为 exe 路径的控制台窗口，先出现而且尺寸不小，很容易抓错。

## 打包

    powershell -ExecutionPolicy Bypass -File packaging\build-release.ps1

会自动从 Cargo.toml 读版本号，产出 dist\FrameGen-Manager-v<版本>.zip 和 SHA256SUMS.txt。
装了 Inno Setup 6 的话还会顺带出安装包。

**注意**：packaging 下的 .ps1 和 .iss 必须保持 **UTF-8 BOM**，
否则 PowerShell 5.1 和 Inno Setup 会按 ANSI 解码，中文变乱码并报语法错。

## 实测指标（release，本机 RTX 3070）

| 指标 | 目标 | 实测 |
|---|---|---|
| exe 体积 | 安装包 < 10 MB | 6.42 MB |
| exe 运行库依赖 | 不要求用户装 VC++ | 17 个系统 DLL，无 vcruntime140.dll |
| 启动 | < 1 s | 约 170 ms |
| 关闭即退出 | 是 | 是 |
| 内存 | < 60 MB | 私有工作集约 68.5 MB，未达标 |

内存超标的部分几乎全部来自中文字体：加载 simhei.ttf（9.7 MB）会多占约 19 MB
私有内存。要达标需要做字体子集化（build.rs + subsetter）。
设 DLSSG_NO_CJK_FONT=1 可跳过字体加载，用来量化这部分开销。

口径提醒：这里的「内存」指**私有工作集**（任务管理器「内存」列那个数），
不是 `Process.WorkingSet64` —— 后者含共享 DLL 页，会虚高 30 MB 左右。
要用 `Get-CimInstance Win32_PerfFormattedData_PerfProc_Process` 的
`WorkingSetPrivate` 字段读才可比。实测：带字体 68.5 MB，DLSSG_NO_CJK_FONT=1 时 49.9 MB。
显卡名伪装功能加入前后用同一套方法各测一次：68.4 MB vs 68.3 MB，没有可测量的差异。

## 上游（sdli1995/dlssg_for_sm86）的两个事实

1. **上游没有 GitHub Releases，也没有 Tags。** 文件直接提交在 main 分支根目录。
   所以「查 Releases」这条路本来就不存在；变更指纹改用 raw.githubusercontent.com
   的 ETag —— 对文件发一次 HEAD 就能拿到，且不占 API 配额。
2. **altnative/ 下的四个 DLL 不是 version.dll 改个名**，而是导出名不同的独立二进制
   （体积都不同）。选了非默认入口就必须下载对应那一个。

## 实现说明

### 自动判断代理入口

五个入口（version / winmm / dinput8 / winhttp / dxgi）是同一个 Mod，选错只会不生效，
不会损坏游戏。判断依据不是游戏名字，而是**导入表**：Windows 加载 DLL 时优先搜索 EXE
所在目录，所以只要游戏会加载的某个模块导入了 version.dll，把代理放进去就会被加载。

判定顺序：

1. 目录里已有本项目的文件 -> 直接复用那个入口（避免同时存在两个代理）
2. 排除被第三方占用的入口
3. 按导入表匹配，优先级 version -> winmm -> dinput8 -> winhttp -> dxgi
4. 都判不出来就用上游默认 version.dll，并在界面上明确说明「未能自动判定」

PE 解析是**随机读取**的：先读头部拿节表，再按节表把 RVA 换算成文件偏移 seek 过去读。
早先的实现是「读文件前 N MB」，对 232 MB 的 TslGame.exe 和 457 MB 的 HogwartsLegacy.exe
完全失效。另外必须同时读**延迟导入表**（DataDirectory[13]），否则会漏掉大部分依赖。

### 识别用户是否已经手动装过

本项目的 5 个 DLL 都用 **DLSSG Native Project** 自签证书签名，这是比文件名或体积
可靠得多的身份标记。工具会读目标目录里已有文件的 PE 证书表（DataDirectory[4]，
注意这一项的 RVA 直接就是文件偏移）：

- 签名者是 DLSSG Native Project -> 本项目的文件，允许安全覆盖
- 其他签名者 / 无签名 -> 第三方文件，拒绝覆盖

### 游戏图标

从**渲染 EXE** 提取（Windows Shell API + GDI），不管游戏来自 Steam、Epic 还是手动添加
都能取到。踩过的坑：SHGetFileInfoW 遇到混合分隔符路径（d:/steam\...）会直接失败，
而 Steam 注册表里的 SteamPath 就是带正斜杠的，所以要先归一化。

### 下载与备用源：正常流程 0 次 API 调用

**这是本模块最重要的一条设计约束：正常使用全程不碰 api.github.com。**

原因：未登录的 GitHub API 按 IP 每小时只有 60 次配额。很多用户走加速器 / 代理，
出口 IP 是共享的，配额会被别人吃光，于是「检查更新」「下载资产」直接失败，
而用户自己完全无从排查。所以整套流程都换成了没有配额的通道：

| 用途 | 原来 | 现在 |
|---|---|---|
| 变更指纹 | contents API 的 blob sha（1 次/目录） | 对 raw 发 HEAD 读 ETag（0 次） |
| 下载 Mod 文件 | raw.githubusercontent.com | 同左（0 次） |
| 下载运行库 | releases API 查资产列表 | 直拼 releases/download/{tag}/{asset}（0 次） |

几条实测结论，别凭直觉改：

* raw 的 ETag 是 64 位十六进制，但它**不是内容的 SHA-256**，也和 git blob sha1 对不上
  （sha256(blob N\\0+content)、sha256(blob N+content)、sha256(hex(blob sha1)) 三种都试过，
  全不匹配）。它是 GitHub 内部的不透明哈希，**只能当变更指纹，不能当内容哈希算**。
* 这个 ETag 在 HEAD 和 GET 上一致；**但只有部分镜像会透传它**。ghproxy.net 会
  （连 X-Served-By 都带过来），而 **gh-proxy.com 不会** —— 走后者时 ETag 比对会落空，
  只剩 Content-Length 校验。所以下面「镜像优先就必须配签名校验」是一条硬要求。
* 运行库的资产名（nvngx_dlssg_310.9.1.zip 等）写死在 DLSS_RUNTIME 表里；
  上游改名 / 删包导致直链失败时，才回退去问一次 releases API。

#### 镜像排序，以及「快」和「可信」的分工

`MIRRORS` 按**实测速度**排（2026-09，拉 raw 上的 version.dll，8 MB 样本，跑两轮）：

| 源 | 速率 |
|---|---|
| gh-proxy.com | 3.9 ~ 5.2 MB/s |
| ghfast.top | 0.5 ~ 0.9 MB/s |
| ghproxy.net | 0.02 ~ 0.16 MB/s（原来内置的是它） |
| raw 官方直连 | 0.00 ~ 0.06 MB/s（基本不通） |

48 MB 资产：ghproxy.net 要十几分钟，gh-proxy.com 十几秒。随时可以用
`cargo run -- --speedtest` 复测 —— 它走的就是界面那条生产路径。

难点是**快的不可信、可信的不快**，所以探测和下载的优先级是**反的**：

* **探测（HEAD，取 ETag 指纹）走官方优先。** 指纹从 GitHub 自己那里拿才可信；
  HEAD 很小，官方 raw 就算链路很慢也能秒回。
* **下载走镜像优先。** 镜像快几十倍，内容再拿官方指纹校验。

gh-proxy.com 不转发 ETag，等于下载侧少了一道校验，所以 `download_auto()` 强制
要求调用方传 `verify` 回调，并按文件类型分开处理：

* **代理 DLL（15 MB，prefer_mirror = true）**：下完必须
  `identify_dll().is_ours()` —— 这 5 个 DLL 都由 DLSSG Native Project 自签，
  镜像伪造不出来。签名不过就丢弃并换源重试。
* **ini（581 B，prefer_mirror = false）**：走官方优先。官方再慢也是瞬间，而且**会**
  返回 ETag，比对能真正生效。ini 没有签名，这是它唯一的校验手段。

界面上的「改用备用源重试」现在只是手动兜底；正常路径会自动多源回退，不用用户点。

**源偏好是有记忆的**：`try_sources()` 记住每个「主机 + 用途」上一次哪个候选成功，
下次从它开始试（`PREF` 四个槽：raw / github.com × 探测 / 下载）。
不记的话，release 直链用的 github.com 每次都要先干等连接超时才轮到镜像。

**探测请求必须带单个请求超时。** raw 现在会**间歇性卡十几秒** —— 同一个 HEAD 连测三次：
19.4s 超时 / 0.6s 成功 / 19.8s 超时。所以「官方优先」只在它答得快时才有意义：
每个 HEAD 限 6 秒，`fetch_version()` 限 10 秒，超了就换镜像。
只调连接超时（`connect_timeout`）是没用的 —— 连接建起来了、响应不来，那就卡到总超时。
实测「取指纹」这一段从 **29.6s 降到 4.8s**。

`fetch_version()` 也走多源回退：它只是给界面显示一个版本号，不该因为 raw 卡住而
把整个「检查更新」拖住。

### 下载慢：看门狗 + 实测速率记忆

**镜像的快慢按用户线路和时间剧烈变化。** 实测同一台机器、同一个 gh-proxy.com，
相隔一小时能从 6.9 MB/s 掉到 0.34 MB/s；同一时刻 ghfast 0.06、ghproxy 0.026。
原来的逻辑只记「上次哪个源成功了」，根本察觉不到这种变化，用户就得陪着慢源
一直等到下完 —— 这就是「备用源也慢」的真正原因。

三件东西配合解决：

1. **看门狗**（`WATCHDOG_*`）。下载中前 3 秒不判（TLS 握手 + TCP 慢启动都要时间），
   之后一旦实测速率低于 `min_speed_kbps`（默认 300，见配置文件）就立刻放弃这个源换
   下一个，而不是把 30 MB 慢慢拖完。小于 4 MB 的文件（581 B 的 ini）不判速 ——
   秒下完的东西判了没意义。
2. **实测速率记忆**（`source_speed.json`，在数据目录）。下载成功和手动测速都会记下
   这个源的实测 KB/s。排序见 `rank_by_scores`：确认够快的在前（越快越前）、没测过的
   居中、确认太慢的垫底。记录 6 小时后过期 —— 镜像速率变得比这还快。
3. **测速 + 用户自己选**。界面上「上游资产」卡片里有「测速」，对每个候选源拉 512 KB，
   把实测速度列出来让用户点。**为什么让用户选而不是程序自动定**：镜像快慢是按用户
   自己的线路变的，开发者这边测出来的名次对他们没有参考价值。

选中的源排最前面，其余镜像仍然兜底（不是「只用它」）。

**全部源都低于阈值时的兜底**：不能就这么失败。`download_auto` / `download_with_mirror`
走两遍 —— 第一遍按阈值挑，一个都没过（记下 `TOO_SLOW_PREFIX`）就把看门狗关掉再走
一遍。慢一点也总比下不下来强，用户至少还有取消按钮。

**并发分片为什么没做**：实测过，在当前链路上反而更慢（单流 2 MB = 1.04 MB/s，
4 × 512 KB 并发 = 0.46 MB/s）。三个镜像都支持 HTTP Range（206），技术上可行，
但数据说明瓶颈不在单连接被限速，而在镜像侧限速或链路本身，所以不做。

**已知没覆盖的**：硬卡死（服务器中途不再发字节）靠客户端 300 秒总超时兜底，不是靠
看门狗 —— reqwest 的阻塞式 `Read` 没法在读中途超时。用户随时可以点取消。

### 改版本号（这个坑踩过两次）

**别用 PS 的 Get-Content / Set-Content 去改 Cargo.toml。** PS 5.1 按 ANSI 码页读无 BOM 的
UTF-8 文件，中文注释里全角句号后面紧跟 CRLF 时，那个  会被当成双字节字符的尾字节吃掉
—— 注释行和下一行合并，Cargo.toml 立刻变成非法 TOML（`cargo metadata` 报
"key with no value, expected ="），而且 Set-Content 还会把已经乱掉的文本再二次编码一遍。

用 .NET 显式指定编码，并且路径给绝对的（Set-Location 不影响 .NET 的当前目录）：

    $noBom = New-Object System.Text.UTF8Encoding($false)
    $t = [System.IO.File]::ReadAllText($abs, [System.Text.Encoding]::UTF8)
    [System.IO.File]::WriteAllText($abs, $t.Replace('0.2.0', '0.3.0'), $noBom)

Cargo.toml 用 $false（不加 BOM）；installer.iss 用 $true（要 BOM，Inno Setup 才认中文）。
改完拿 `cargo metadata --no-deps` 验一下版本号读出来对不对，别等编译到一半才发现。

### 软件自身更新检查（别和上游 Mod 的更新混了）

两套东西：

* **上游 Mod 更新**（sdli1995/dlssg_for_sm86）—— 只在用户点「检查更新」时跑，走 raw 的 ETag。
* **本软件更新**（XiaoQAQ10086/FrameGen-Manager）—— 启动时自动跑一次，另外标题栏
  有个「检查更新」可以手动再查。这一项默认就开，界面上不设开关，入口也别放回资产
  卡片里 —— 放那儿会被当成上游 Mod 的更新。

本软件更新**读的是我们自己仓库 main 分支上的 Cargo.toml 的 version 字段**。为什么不用
Releases 接口：

* api.github.com 未登录按 IP 限 60 次/小时 —— 正是这个项目一直在躲的东西；
* gh-proxy 这类镜像**只代理资源文件、拒绝代理网页**（实测直接回
  "Web page content is not allowed"），所以 releases 页面和 releases.atom 都拿不到；
* raw 上的 Cargo.toml 只有 2KB，官方源和镜像都拿得到，且不占配额。

**前提**：发布流程必须是「改版本号 -> 提交 -> 打标签 -> 发 Release」一条龙，
这样 main 上的版本号才等于最新已发布版本。改了发布流程就要回来改这里。

**取所有源里最大的版本号，不只信第一个成功的源。** raw 前面有 CDN 缓存，仓库里刚改完
Cargo.toml 的那几分钟，缓存还在吐旧内容。实测发 0.4.0 时官方 raw 有约 3 分钟仍然说
0.3.0 —— 而官方是优先源，一旦它「成功」返回就直接采信了，那段时间 0.3.0 的用户会被
告知「已是最新」，根本看不到更新提示。所以 `fetch_latest_self_version()` 改成**并发问
所有源、取最大的那个**（并发所以耗时约等于最慢的那一个请求，不是相加）。挑最大值的
`pick_latest()` 是纯函数，`--selfupdate` 里有 4 条断言盯着它。

版本号比较用手写的三段数字比较（`0.2.10 > 0.2.9` 这种要正确），解析不出来时
一律当作「没有新版本」—— 宁可漏报也别误报。`--selfupdate` 里也有边界用例。

### 资产状态与部署的几条规矩

**已经是新版就别重下。** 「下载 / 更新资产」对每个文件先过
`update::local_is_current()`：必须同时满足「下载记录在 + 指纹和远端一致 + 文件真的还在
且大小对得上」，三条全中才跳过。只信记录会造成「文件被删了却认为无需下载」——
和 asset_state 是同一个坑，`--downloadtest` 里两条断言专门盯着它。
15 MB 的代理 DLL 因此不用每次重拉。

**资产状态以磁盘为准。** `App::asset_state()` 必须先确认文件真的还在 assets 目录里，
再去看 update_state.json 里的下载记录。只信记录会造成「用户把资产删光了，界面还显示
已就绪」—— 记录在，文件早没了。判定顺序：文件不存在 -> 未下载；大小与记录不符（被改过
或没下完）-> 有更新；再比 ETag。

**重部署必须沿用第一次的备份。** 上游更新后用户会再点一次「部署」。如果这时把「我们
上次部署进去的文件」当成游戏原文件重新备份，就会覆盖掉真正的原件，之后「还原」只能
还原出我们自己部署的那一版 —— 原件永久丢失。所以 `deploy()` 会先查旧 manifest，
已经有原始备份的条目直接沿用，绝不重下备份。备份文件本身丢了的才重新备份。

**换代理入口要清掉旧的，而且原件不能失联。** 用户手动换入口再部署时，旧的那个会留在
游戏目录里 —— 两个代理并存，游戏加载哪个全看运气；而且新 manifest 里没有它，就变成
「还原也管不到」的孤儿。规则集中在 `plan_orphan()` 里，判定故意保守：必须是旧 manifest
记过账、文件还在、而且当前内容仍等于我们当初写进去的那份（用户换过就不碰）。分两种：

* 那个位置**本来空着**、是我们放进去的 -> 直接删掉。
* 那个位置**原本就有文件**（用户手动装过、或旧版本装的同项目文件）-> 也要删掉（否则两个
  代理并存），但必须把那条 `BackupEntry` **搬进新 manifest 再写回**，否则「还原」就再也
  放不回原件。

第二种以前是直接 `continue` 跳过的 —— 结果是两个代理并存 + 原件永久失联。改这条时特意
把 `plan_orphan()` 抽成纯函数，因为真实的代理 DLL 带本项目签名、自测里造不出来：决策靠
--deploytest 里那 6 条纯逻辑断言覆盖，再用游戏目录里那份真实签名 DLL 跑一遍端到端。

### 显卡名称伪装（注册表）

只改 `HKLM\SYSTEM\CurrentControlSet\Enum\PCI\<设备>\<实例>\DeviceDesc` 一个值。
几条不能破的规矩：

1. **只写 DeviceDesc。** `HardwareID`、`CompatibleIDs`、`Driver`、`Service`、`Mfg`
   一律不碰 —— 改这些会让驱动绑定失效。
2. **三重过滤定位目标**：`VEN_10DE` 前缀 + `ClassGUID={4d36e968-...}` +
   `Service=nvlddmkm`。只按 `VEN_10DE` 匹配会把 NVIDIA 高清音频控制器一起改掉
   （DEV_228B 就是音频设备，ClassGUID 是 {4d36e97d-...}）。
3. **故意不改类键的 `DriverDesc`。** 本程序自己的 SM86 / SM75 路由判断读的就是它；
   改掉之后程序会把 RTX 30 系误判成「RTX 50 系，不需要本 Mod」。
4. **写之前必须备份并读回校验。** 备份落在程序同级的 `backups\gpu-name`，
   存的是「第一次改动之前」的值，之后重复改不会把它覆盖掉。
5. **型号只能从 `gpu::PRESETS` 里选**，`apply()` 会再校验一次，不在名单里直接拒绝。

还原有两种目标：`RestoreTo::DriverName`（回到驱动记录的真名，彻底去掉伪装）和
`RestoreTo::BackupOriginal`（回到本工具动手之前的值）。在「之前已被别的工具改过」的
机器上这两者不一样，所以界面上必须让用户自己选，不能替他决定。

提权走 `ShellExecuteW("runas")` 重新拉起自身，**不申请 UAC 清单** —— 主程序保持免提权，
便携运行才不会每次启动都弹框。见 `gpu::run_elevated()`。

驱动版本：优先问 `nvidia-smi`（驱动自报，约 35 ms），读不到就解析注册表
`DriverVersion`（`32.0.16.1692` -> 取数字后 5 位 -> `616.92`，已用 nvidia-smi 交叉验证）。
阈值是 `gpu::MIN_FG_DRIVER = (591, 86)`。

## 数据存放策略

assets / backups / 配置文件都优先放在 **exe 同级**（便携，解压即用，拷走就带走全部状态），
该目录不可写时才回退 %APPDATA%。可写性判断走 `util::is_writable()` —— 真去写一个探针
文件，而不是只看只读属性，因为 ACL 挡住的写操作从只读属性上看不出来。

早期版本把备份放在 `%APPDATA%\FrameGen-Manager\backups`，现在由
`util::migrate_backups()` 在 `main()` 开头做一次性搬迁：只在「新位置为空」且
「老位置有东西」时搬，**全部复制成功才删老目录**，任何一步失败都保留老目录。
结果缓存在 `OnceLock` 里，界面和命令行拿到的是同一份。

## 目录结构

    src/
      main.rs        UI + 后台线程 + 命令行自测入口
      deploy.rs      部署 / 备份 / 还原（多文件 + manifest）
      scan.rs        Steam/Epic 扫描 + 最小 VDF 解析 + PE 解析 + 入口推荐 + 显卡路由识别
      gpu.rs         驱动版本检测 + 显卡注册表实例枚举 + 名称伪装 / 备份 / 还原 + 提权
      anticheat.rs   反作弊检测（注册表服务 + 游戏目录特征）
      update.rs      上游更新检查 + 下载 + zip 解压 + INI 改写
      icon.rs        从 EXE 提取图标
      theme.rs       浅色主题 + 卡片 / 徽章 / 按钮组件
      util.rs        哈希 / 原子替换 / 便携配置与备份目录（含老版本一次性搬迁）
