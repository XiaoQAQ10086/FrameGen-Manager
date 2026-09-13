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
* 这个 ETag 在 HEAD 和 GET 上一致；ghproxy 镜像会把 GitHub 的响应头原样透传
  （连 X-Served-By 都在），所以走镜像时也能拿到同一个 ETag 用于比对。
* 因此下载校验 = 「HEAD 拿到的 ETag」对比「GET 响应里的 ETag」，外加 Content-Length 比对。
  官方源直连时 HTTPS 本身已保证内容真实性，这一步主要防镜像返回错东西。
* 运行库的资产名（nvngx_dlssg_310.9.1.zip 等）写死在 DLSS_RUNTIME 表里；
  上游改名 / 删包导致直链失败时，才回退去问一次 releases API。

官方源不可达时界面会给「改用备用源重试」，前缀 https://ghproxy.net/。
本地文件状态完全按本地判断，不依赖网络。

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
4. **写之前必须备份并读回校验。** 备份落在程序同级的 `backups\gpu-name\`，
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
