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
    cargo run -- --sourcetest      # 选源排序 + 慢源必须下完（本地起慢服务器，不依赖外网）
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
2. **备用代理 DLL 不是 version.dll 改个名**，而是导出名不同的独立二进制（体积都不同）。
   选了非默认入口就必须下载对应那一个。
3. **上游 0.3.0 换过一次仓库布局**（native 模式改回代理模式），一次断过我们好几处：
   签名证书、备用入口目录名、INI 键、版本号写法全变了。详见下面「上游 0.3.0 改版」一节。

## 上游 0.3.0 改版（native -> 代理模式）

上游把项目从 native 模式改回代理模式，版本号 0.3.0，旧的 native 版挪进了
archive/0.2.4/，仓库根目录现在就是新版。这一版变了五处，每一处都会让老代码出错：

| 变了什么 | 老代码的假设 | 现在的样子 | 不改会怎样 |
|---|---|---|---|
| 签名证书 | DLSSG Native Project | DLSSG for SM86 | 所有下载被自己的签名校验拒掉 |
| 备用入口目录 | altnative/ | alternatives/ | 探测 404，备用入口永远下不到 |
| 备用入口名单 | winmm/dinput8/winhttp/dxgi | winmm/dbghelp/dinput8/dxgi/d3d12 | winhttp 已删、dbghelp/d3d12 漏掉 |
| INI | 有 Router / KernelImage / HardwareBilinear | 精简成 5 段约 2 KB，没有 Router | SM75 改写抛错，RTX 20 用户部署直接失败 |
| README 版本号 | "# DLSSG Native 0.2.4" | "# DLSSG for SM86（Proxy）- 0.3.0 版本" | 界面「上游版本」显示未知 |

因此代码里现在有两套「资产版本」：

* **最新版**（默认）：仓库根目录 + alternatives/，后端是 310.9，面向 RTX 30 系。
* **310.1 版**：310.1/ + 310.1/alternatives/，后端是 310.1 —— 二进制里带
dlssg-310.1-d3d12-sm86+sm75、sm75_route_limits、sm75_slots，即**带 SM75 内核**，
给 RTX 20 系用。INI 仍然用根目录那份（出厂 INI 自己会按内嵌运行库钳倍率：
310.9 钳到 6X、310.1 钳到 4X）。入口是「部署」卡片里给 SM75 机器显示的按钮，
状态存在配置的 `legacy_3101`。

判断依据（翻的是程序本体里的字符串，不是猜的）：根目录那份写着
"The 310.9 backend has no SM75 kernel family; use Router=Auto or SM86"，
310.1 那份没有这句话，而且文件大 1.4 MB（正好多一个内核族）。

**GTX 16 系单独成一条路（`GpuRoute::Gtx16`）并且禁止部署**：1630 / 1650 / 1660 和
RTX 20 系同为 Turing，但**没有 Tensor Core**，DLSS 帧生成在硬件上就跑不了 ——
换 310.1 版也没用。所以它**不能**落到 `GpuRoute::Sm75`：那会给出「改用 310.1 版」
这个根本无效的建议。`scan::classify_gpu()` 里 GTX 16 的判断必须排在 RTX 20 前面。

路径由 `update::proxy_repo_path(proxy, legacy)` 和 `update::ini_repo_path(legacy)` 统一决定，
入口名单只有一份 `scan::PROXY_ALL`（`PROXY_PRIORITY` / `PROXY_HISTORIC` 是它的两个视图，
  两个版本的目录结构相同）—— 要改只动这几处，
别再散落硬编码（0.7.0 那种写死名单的写法正是这次集体 404 的原因）。
老名字（winhttp.dll）留在 `scan::PROXY_HISTORIC` 里，只用于「这算不算代理入口」的判断。

版本号解析（`update::extract_version`）先按 "Native " 锚点找，找不到再取首行第一个
「带小数点的数字」，两种写法都能认；`--selftest` 里有这两种格式的断言。

## 实现说明

### 自动判断代理入口

每个入口（version.dll，以及备用目录里的 winmm / dbghelp / dinput8 / dxgi / d3d12）
都是同一个 Mod，选错只会不生效，不会损坏游戏。判断依据不是游戏名字，而是**导入表**：Windows 加载 DLL 时优先搜索 EXE
所在目录，所以只要游戏会加载的某个模块导入了 version.dll，把代理放进去就会被加载。

判定顺序：

1. 目录里已有本项目的文件 -> 直接复用那个入口（避免同时存在两个代理）
2. 排除被第三方占用的入口
3. 按导入表匹配，优先级见 scan::PROXY_PRIORITY：
   version -> winmm -> dbghelp -> dinput8 -> dxgi -> d3d12
   （310.1 版的目录结构相同，所以两个版本共用这一份清单）
4. 都判不出来就用上游默认 version.dll，并在界面上明确说明「未能自动判定」

PE 解析是**随机读取**的：先读头部拿节表，再按节表把 RVA 换算成文件偏移 seek 过去读。
早先的实现是「读文件前 N MB」，对 232 MB 的 TslGame.exe 和 457 MB 的 HogwartsLegacy.exe
完全失效。另外必须同时读**延迟导入表**（DataDirectory[13]），否则会漏掉大部分依赖。

### 识别用户是否已经手动装过

本项目的 DLL 都用自签证书签名，这是比文件名或体积可靠得多的身份标记。工具会读目标
目录里已有文件的 PE 证书表（DataDirectory[4]，注意这一项的 RVA 直接就是文件偏移）：

- **两个证书名都算「本项目的文件」**：DLSSG Native Project（0.2.4 native 包）和
  DLSSG for SM86（0.3.0 代理包）。上游 0.3.0 换了证书而代码只认旧名字，结果所有
  下载都被自己的校验拒掉 —— 用户看到的正是「判定为其他签名者，已丢弃并换源重试」。
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
  `identify_dll().is_ours()` —— 这些 DLL 都由上游自签（两个证书名，见上文），
  镜像伪造不出来。签名不过就丢弃并换源重试。
* **ini（2 KB / 581 B，prefer_mirror = false）**：走官方优先。官方再慢也是瞬间，而且**会**
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

### 下载慢：让慢源下完，只在卡死时换源

**先记一条反常识的结论：不要再按速度换源。** 曾经做过「看门狗」—— 下载中一旦实测
平均速率低于 `min_speed_kbps`（默认 300 KB/s）就立刻放弃这个源换下一个。
结果有用户反馈「下载到四分之一就断了」：他家线路就是 200 多 KB/s，每个源都被判成
「太慢」，换完一圈还是下不下来。现在这套阈值逻辑整组删掉了。

**镜像的快慢按用户线路和时间剧烈变化。** 实测同一台机器、同一个 gh-proxy.com，
相隔一小时能从 6.9 MB/s 掉到 0.34 MB/s；同一时刻 ghfast 0.06、ghproxy 0.026。
原来的逻辑只记「上次哪个源成功了」，根本察觉不到这种变化，用户就得陪着慢源
一直等到下完 —— 这就是「备用源也慢」的真正原因。

现在只保留两件事：

1. **实测速率记忆**（`source_speed.json`，在数据目录）。下载成功和手动测速都会记下
   这个源的实测 KB/s。排序见 `rank_by_scores`：确认够快的在前（越快越前）、没测过的
   居中、确认太慢的垫底。记录 6 小时后过期 —— 镜像速率变得比这还快。
   **它只决定先试谁，不决定放弃谁**（排序分界线是 `GOOD_SPEED_KBPS`）。
2. **测速 + 用户自己选**。界面上「上游资产」卡片里有「测速」，对每个候选源拉 512 KB，
   把实测速度列出来让用户点。**为什么让用户选而不是程序自动定**：镜像快慢是按用户
   自己的线路变的，开发者这边测出来的名次对他们没有参考价值。

选中的源排最前面，其余镜像仍然兜底（不是「只用它」）。

**防卡死靠客户端超时，不靠速度阈值。** reqwest 的阻塞读每调用一次就重新计时
（`blocking::Response::read` 里是 `wait::timeout(..., timeout)`），所以客户端上那个
300 秒其实是「单次读取」的上限：只要服务器还在往外吐字节，多慢都能慢慢下完；
连续 300 秒一个字节都没有才判这个源死了、换下一个。

因为不再有「太慢」这个失败原因，`download_auto` / `download_with_mirror` 里那套
「全部源都太慢 -> 放宽速度要求再走一遍」的第二遍也一并删了 —— 两遍已经完全一样，
留着只是把同一份文件下两次。

回归用例在 `--sourcetest`：本地起一个约 220 KB/s 的慢源，4 MB 的文件必须完整下完、
字节数要对。这条就是为了钉死「慢源被掐」这个毛病。

**并发分片为什么没做**：实测过，在当前链路上反而更慢（单流 2 MB = 1.04 MB/s，
4 × 512 KB 并发 = 0.46 MB/s）。三个镜像都支持 HTTP Range（206），技术上可行，
但数据说明瓶颈不在单连接被限速，而在镜像侧限速或链路本身，所以不做。

**已知没覆盖的**：服务器以极低速度持续吐字节（比如 1 KB/s）时不会自动放弃，会一直
慢慢下 —— 这是「速度慢就慢吧」的刻意取舍，用户可以随时点取消。

### 游戏库缓存（下次打开直接显示）

扫描很贵：找渲染 EXE 要遍历游戏目录、反作弊要扫一遍、图标要从 EXE 里提。所以结果存成
game_library.json（和配置文件放一起，便携版就在 exe 旁边）：条目 + 渲染 EXE 路径 +
反作弊等级 + 上次扫描时间。启动时读它、重建列表（部署状态和图标现算，很快），
**不做后台轮询** —— 想刷新还是点按钮。

* 扫出来的条目：安装目录不在了就丢掉（游戏卸载了）。
* 手动条目（用户自己「存到游戏库」的）：目录不在也留着，让他自己决定要不要移除
  （可能只是移动硬盘没插）。
* 两者分开存两个数组：**重新扫描只覆盖 scanned，绝不碰 manual**。
* 手动条目的部署目标就是用户选的那个目录本身，不去猜「渲染 EXE 在哪一级」；
  手动条目即使找不到渲染 EXE 也允许部署。
* **每次部署 / 还原之后必须重算每一行的部署状态**（refresh() 里那段循环）。行上的徽章是
  建行时算出来的，不重算就会停在部署前：用户明明部署成功（左侧卡片已是「已部署」），
  行上却还写着「非本工具部署」，看上去就是部署没生效 —— 有用户报过。
  部署 / 还原的日志里现在会记「备份记录可读=」，这类投诉看一行就能定论。

### 手动导入（网盘救急）与严格签名校验

**为什么要有**：镜像和 raw 全慢到不可用时（实测有 0.02 MB/s 的），程序自己下不动 84 MB 资产。
所以给一条绕开网络的路：用户拿 zip，程序负责解压 + 校验 + 落位。

* `importer::stage()` —— 用 `update::zip_list()` / `zip_extract_to()` **流式**解
  （不能整包读进内存：上游源码包一百多 MB），按**文件名**递归认（大小写不敏感）：
  7 个代理入口 + ini + 2 个运行库，其它文件一律忽略。认出来的逐个校验。
* `verify::verify_file()` —— 严格版，和 `scan::identify_dll()` 分工不同，**别混用**：
  * `identify_dll` 宽松，只回答「能不能安全覆盖」（证书里出现签名字符串就算本项目的）。
    它**能被自签一张同名证书骗过去**，所以不能拿来做导入校验。
  * `verify_file` 用 `WinVerifyTrust` 验内容（改一个字节就变 `TRUST_E_BAD_DIGEST`）+
    `CryptQueryObject` 取**真正签名的那张证书**（不是证书包里随便一张 —— 否则塞入作者证书、
    再用别人私钥签名就能骗过去）再比 SHA-1 指纹。
    NVIDIA 走公开 CA：`Trusted` + 主体是 NVIDIA 即可（换证书不用改代码）。
  * 两个指纹常量在 `verify.rs` 顶部；作者换证书时，那种文件会落到「弹窗可继续」那一档，不会挡死。
* ini 没签名，只能结构检查（`importer::ini_sanity`）。
* 策略：签名可信 → 静默；否则 → 弹窗（默认跳过可疑项，可强行导入，选择写日志）。
* 导入的文件在 `update_state.json` 里标 `imported: true`：判定「已就绪」只比文件在不在、
  大小对不对，**不再要求官方指纹** —— 用户就是因为下不动才导入的，否则会又去下一遍。
* 自测：`--importtest <zip>`（只解包 + 校验，不写资产目录）、
  `--verifytest <文件>`（打印签名结论、签名者、证书指纹）。

### 下载源列表（为什么改成一行下拉）

平铺按钮在 3 个源时刚好，6 个以上会折成好几行 —— 改成一行 `ComboBox`，「自动」时把实测最快的那个
写进标题。源列表 2026-09-15 用 `--speedall` 实测两轮共 27 个候选，活下来的只有 6 个（写进
`MIRRORS` 并注明实测速度）；**死源不留** —— 每次下载都要为它空等一次超时。**下载源只能从这几个
里挑**（「自动」或指定其中一个），手填地址那个功能已经删掉；`update::normalize_source()` 只负责
给选中的前缀做归一化（补结尾斜杠）。

### 两个 DLSS 运行库：缺才补，已有不动

**用户反馈**：用工具部署后帧生成不生效，手动只复制「代理 DLL + INI」进游戏目录却正常。

原因就在这：`do_deploy` 以前**无条件**部署 4 个文件（代理 + INI + nvngx_dlssg.dll + nvngx_dlss.dll），
而目标目录已有同名文件时，只要对方是 NVIDIA 签名就**直接覆盖（先备份）**。可很多游戏自带的那两个
运行库是跟它自己的 DLSS / Streamline 版本配套的，覆盖之后就对不上套了 —— 而上游 0.3.0 的说明里
本来也只要「代理 DLL + INI」（运行库/模型/后端都内嵌在代理里）。

现在：`update::runtime_deploy_plan(target)` 返回 (已有的, 缺的)，**已有的一个都不动**、
缺哪个补哪个，界面上写明「游戏目录已有 xxx，用游戏自带的那份」（`deploy_notes`），
状态栏也会带上这句。下载行为不变（两个运行库照样下载到资产目录备用）。

实测（霍格沃茨目录，两个运行库是上次部署留下的）：

```
[04:38:52] 游戏目录已有 nvngx_dlssg.dll，用游戏自带的那份（不覆盖）
[04:38:52] 游戏目录已有 nvngx_dlss.dll，用游戏自带的那份（不覆盖）
[04:38:52] 已部署 2 个文件 -> …：version.dll、dlssg_sm86.ini
（文件时间：ini 与 version.dll 更新了，两个运行库保持原时间 → 确实没被碰）
```

自测里 `--selftest` 有三条断言覆盖：两个都缺 → 都补；只有一个 → 只补另一个；两个都有 → 一个都不动。

### 日志只留两个文件（framegen.log / framegen.prev.log）

早先是「一次运行一个带时间戳的文件，保留 10 个」，用户反馈说点开一次多一个、看着乱；
现在改成**固定名字滚动**（`log::rotate`）：启动时把上一次那份改名成 `framegen.prev.log`、
更早的删掉、本次写 `framegen.log` —— 目录里永远最多两个文件。

为什么不只留一个：用户常常先关掉程序、过一会儿才来反馈，只留一个的话出问题那趟已经被
这次启动覆盖了。留「上一次」刚好够查又不堆积。

升级兼容：老版本留下的 `framegen-<时间戳>.log` 在第一次滚动时会把**最新的一份**留成
`framegen.prev.log`，其余删掉 —— 用户升级上来目录立刻从十几个变成两个，而不是继续留着
（实测：11 个 → 2 个）。`log::rotate` 抽成独立函数就是为了自测能直接验这三种情况
（升级清理 / 正常滚动 / 连续滚动不堆积）。

### 扫描的日志（用户报「扫不出来」时靠它）

`scan_all_notes()` 把每个平台的过程说明既写进日志、也带回界面的操作日志：

* 每个 Steam 根目录**是从哪来的**（HKCU SteamPath / HKLM 32 位或原生 InstallPath /
  SteamExe / 卸载项 InstallLocation / 默认路径）。以前只读两个注册表键，装了两份 Steam
  或键位置不同的机器会整库漏掉。
* 每个根目录下有几个库目录（来自 libraryfolders.vdf）。**读不了库清单会明确写出来** ——
  这是「只扫出几个游戏」最可能的成因：读不到库清单就只知道默认库，其它盘的游戏全消失，
  而旧代码是静默的。
* 每个被跳过的条目和原因：运行库、目录不存在、清单读不了（含具体文件名）。
* Steam 运行库过滤改成「固定名 + 版本号前缀」，**不能再用子串匹配**：早先名字里含
  proton 就丢，把 Proton Bus Simulator 这种真游戏一起误杀了（自测里有这条用例）。
  WeGame 侧同理：没装 / 看了几个注册表项 / 因「非游戏键 / 没读到目录 / 目录里找不到游戏程序」
  各跳过多少。

**平台之外的游戏扫不到是设计使然**：鸣潮用官方启动器、网易用游戏中心装的那些，Steam/Epic/WeGame
的清单里没有它们。别去猜硬盘上哪个目录是游戏 —— 正确做法是让用户用「存到游戏库」手动加。

### 选中反馈：高亮 + 飞行动画

点「用作部署目录」原来只有状态栏一行字，用户感知不强。现在：

1. 被选中的那一行一直带「已选中」徽章 + 主题色边框（长期可见，不是一闪而过）；
2. 那张卡片缩小后沿缓动曲线飞到「目标目录」卡片上，落地时目标卡片描边闪 0.8 秒；
3. 点击后的深度反作弊扫描挪到后台线程 —— 以前同步跑，点下去要卡一下，现在立刻有反应
   （徽章先显示「正在后台分析反作弊」）。

动画只在 0.45 秒里请求重绘，结束就停，空闲时一帧都不多画。插值抽成纯函数
`fly_lerp`，自测里验端点，不用开窗口。目标卡片滚出可视区时跳过飞行、只闪一下。

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

**目录里还有本项目的另一个代理时也要问一句。** 上游 README 明确要求「每次只保留
本项目的一个代理」。如果旧的那个是用户**手动**装进去的，manifest 里没有记录，第 8 步
的清理就没有依据可查；而「目录里还有别的东西」那道提醒只针对非本项目文件（怕误删别人
的）—— 两个代理于是静默并存，用户还以为部署成功了。所以 @@start_deploy()@@ 先用
@@find_extra_own_proxies()@@ 查一遍，查到就弹窗，用户确认移除才动手；移除前先备份并记成
@@existed_before=true@@ 的条目，因此「还原」能把原件放回去。（选择"保留并继续"就照旧不动。）

这类记录的**延续**很关键：第 8 步会把「有原件备份、这次又不部署」的条目一直带进新的
manifest，包括文件已经不在了的 —— 因为「还原」收尾会删掉整个备份目录，记录一丢，
原件就永久拿不回来了。@@--deploytest@@ 里有一条专门验"再部署一次记录还在"。

### 硬件加速 GPU 计划（只读 + 跳转，绝不写）

DLSS 帧生成要求系统开启「硬件加速 GPU 计划」（CDPR 官方的 DLSS FG 支持页明确写了这一条；
上游作者的文档反倒一个字没提，所以这是帧生成本身的系统前提，不是这个 Mod 特有的）。

**注册表里只有一个地方记它**：`HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers` 的
`HwSchMode`（2 = 开，1 = 关）。但这个值**可能根本不存在** —— Windows 11 默认就是开启，
系统不一定往注册表写这一项。实测本机 Win11 26300 **实际开着、值却不存在**，而且把整个
`HKLM\SYSTEM\CurrentControlSet` 扫了一遍也没有第二个地方记状态。所以：

* 读到 2 -> 已开启；读到 1 -> 已关闭；**读不到 -> 未知**，绝不能当成"关着"。
* 界面按三态显示，未知时加一句「Win11 默认开启，想确认请点「去设置」」。
* 部署后的提示**只在明确读到 1 时弹** —— 否则 Win11 那些本来就开着的用户每次部署都会被骚扰。

**为什么不写注册表**：写它要管理员权限（UAC）、而且必须重启才生效，而用户自己在设置里点
一下只要一秒。所以这个功能只做两件事：读状态、跳转到设置那一页。不碰系统设置、不要提权、
不涉及重启。跳转走 `ShellExecute`（和打开网址同一套），URI 见微软官方的 ms-settings 列表：
Win11 用 `ms-settings:display-advancedgraphics-default`（"默认图形设置"，开关在这页上），
Win10 用 `ms-settings:display-advancedgraphics`。

调试开关 `DLSSG_FAKE_HAGS=on|off|unknown` 可以强制三种状态，用来截图验证显示；
`--gpuinfo` 里有 5 条断言盯着"值 -> 状态"的翻译；`--openhags` 单独测跳转。

### WeGame 扫描（保守实现，未在真实环境验证）

**上游没有官方文档。** WeGame 把每个游戏的安装位置记在注册表里 —— 网上流传的「重装系统后
重新关联 WeGame 游戏」的土办法就是手工重建这些键，说明这些键就是 WeGame 判断安装位置的
依据。所以 `scan_wegame()` 去枚举 `HKLM\SOFTWARE[\WOW6432Node]\Tencent` 的子键，
从值里找指向真实目录的路径。WeGame 是 32 位程序，所以主要看 WOW6432Node 那一侧。

**开发机上没装 WeGame，所以这部分没有在真实环境验证过**（其他平台都是拿真实机器验的）。
为此判定做得非常保守：**只有目录里真能找到游戏可执行文件才算一条记录**，宁可漏报也不列
垃圾。扫不到不影响使用 —— 界面上还有「选择目录」。

**踩到的坑（自测抓出来的）**：本机装了 QQ，而 QQ 的注册表键**也在 Tencent 下面**，它的
数据指向 QQ 自己的安装目录，里头当然找得到 exe —— 于是一度被当成一条「WeGame 游戏」
列了出来（显示成 "QQNT"）。现在有两道闸：

1. `wegame_installed()` —— WeGame 自己没装就直接返回空，不去 Tencent 键下面瞎猜；
2. `WEGAME_NON_GAME_KEYS` —— 精确匹配（不前缀匹配，免得误杀「QQ飞车」这类真游戏）
   跳过 QQ / QQNT / WeChat / TIM 之类的客户端键。

`--selftest` 里验的是**不变式**（扫出来的每条都必须指向真实存在、且能找到游戏程序的
目录），不是数量 —— 因为本机没有 WeGame，数量恒为 0，验数量没意义。另外还有两条纯函数
断言（`wegame_value_is_path_like` / `wegame_name_from_key`）。

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

assets / backups / 配置文件 / **下载记录**（`assets\update_state.json`）都优先放在
**exe 同级**（便携，解压即用，拷走就带走全部状态），该目录不可写时才回退 %APPDATA%。
可写性判断走 `util::is_writable()` —— 真去写一个探针文件，而不是只看只读属性，
因为 ACL 挡住的写操作从只读属性上看不出来。

唯一的例外是 `source_speed.json`（各镜像的实测速率）：它反映的是**用户自己的线路**，
换台机器就没有意义了，所以固定留在 %APPDATA%。
早先下载记录也固定写在 %APPDATA%，那会导致「文件都在、记录找不到 → 重下 17~19 MB
代理 DLL」，已改成跟着资产走；`update::load_state()` 会从两个老位置
（FrameGen-Manager / DLSSG-Manager）读一次并迁过来，老文件不删。

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

## 图标

* `packaging/app-icon.png` —— 1024×1024 母版（美术原图另存一份留档）。改图后重跑：
  `powershell -ExecutionPolicy Bypass -File packaging\make-ico.ps1`
* `packaging/make-ico.ps1` —— 从母版生成并**提交** `packaging/app.ico`
  （16/20/24/32/40/48/64/96/128/256；≤64 存 DIB、≥96 存 PNG）。Cargo 因此**不需要任何
  图片 / 图标 crate**。脚本顺带把每一档**用 Win32 LoadImage 读回来**验证并出一张预览图。
  （注意：System.Drawing 自己的 `Icon(path,w,h)` 读不了 PNG 压缩帧、会读出噪点 —— 那是
  .NET 的老问题，Shell / LoadImage / PrivateExtractIcons 都正常，别被它误导。）
* `build.rs` —— 调 Windows SDK 自带的 `rc.exe` 把 app.ico 编成 .res，再用
  `cargo:rustc-link-arg` 交给链接器。**没有 build-dependency**；找不到 rc.exe 或 app.ico
  时只警告、不中断构建（换台没装 SDK 的机器照样能编，只是 exe 没图标）。
  坑：`.rc` 里 `\` 是转义字符，路径里的反斜杠必须写成两个，否则 rc 报 RC2135 file not found。
* 窗口 / 任务栏图标不是另找一张图：`icon::app_icon()` 用 `PrivateExtractIconsW` 从
  **本 exe 的图标资源**取 256 档，复用 `icon.rs` 里已有的 HICON → RGBA。
  `--selftest` 里有断言（尺寸 + 像素不是全透明）。
* 安装包：`packaging/installer.iss` 的 `SetupIconFile=app.ico` 用的是同一个文件。
