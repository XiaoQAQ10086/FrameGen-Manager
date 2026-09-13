# DLSSG-Manager 模块开发提示词

每开一个新会话：先发「0. 项目背景包」，再发对应模块那一节。上下文不继承，别指望 Agent 记得。

---

## 0. 项目背景包（每次必发）

仓库：https://github.com/sdli1995/dlssg_for_sm86
作用：DLL 代理，让 RTX 30 系（SM86）/ 20 系（SM75）启用 DLSS 多帧生成。默认分支 main。

分发文件（**直接提交在仓库根目录**，不是 Release 资产）：
- version.dll         15,667,520 字节
- dlssg_sm86.ini             581 字节

替代入口（altnative/，复制时保持原文件名，同一时刻只用一个代理）：
- winmm.dll / dinput8.dll / winhttp.dll / dxgi.dll

历史快照：archive/（含旧版 version.dll 与 ini，可作回退源）

放置位置：游戏**实际渲染 EXE** 所在目录，例如
D:\SteamLibrary\steamapps\common\BlackMythWukong\b1\Binaries\Win64 ，或游戏根目录。

INI 关键项：Router=SM86|SM75、KernelImage=PTX|Cubin、HardwareBilinear=0|1、
MaxGeneratedFrames=1..3、Logging.Level=0..3。运行时日志在 EXE 旁 dlssg_sm86\logs。

风险：内核级反作弊游戏禁止使用（可能封号）；4K 输出额外显存 2X≈700 / 3X≈740 / 4X≈770 MiB；
DLL 用「DLSSG Native Project」自签证书，不自带 Windows 信任，可能被杀软误报。

技术栈硬约束：Rust + egui/eframe。禁止 Electron / Tauri / WebView。
安装包 < 10 MB，空闲内存 < 60 MB，启动 < 1 s，关闭即退出（不驻留、不轮询、不自动全盘扫描）。

已核实的本机环境（可用于自测）：
- Steam 装在 D:\Steam（注册表 HKCU\Software\Valve\Steam 的 SteamPath = d:/steam）
- Epic 清单目录 C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests 存在
- 注册表已存在 BattlEye 服务项 BEService（反作弊模块的真实测试样本）

---

## 1. 部署模块

目标：把 version.dll + dlssg_sm86.ini 部署到选定游戏目录，支持备份与还原。

要求：
1. 用 rfd 的 pick_folder() 选目录。选定后分析该目录，找候选渲染 EXE：优先 *-Win64-Shipping.exe，
   其次任何导入了 d3d12.dll 的 EXE。读 PE 导入表判断，不要靠文件名猜。
2. 部署前冲突检查：目录里若已存在 version.dll / winmm.dll / dinput8.dll / winhttp.dll / dxgi.dll，
   且其签名者不是 "DLSSG Native Project"，判定为入口冲突，列出可用替代入口让用户选，或中止。
3. 占用检查：游戏运行中会锁住 DLL。以独占方式试打开目标文件，失败就提示先退出游戏。
4. 备份：把「本次将新增或覆盖的每个文件」的原状写入
   %APPDATA%\DLSSG-Manager\backups\<game_key>\ ，并生成 manifest.json：

~~~rust
#[derive(Serialize, Deserialize)]
struct BackupManifest {
    game_key: String,          // sha256(规范化 game_dir) 前 16 位十六进制
    game_dir: PathBuf,
    deployed_at: String,       // RFC3339
    proxy_name: String,        // 例如 "version.dll"
    files: Vec<BackupEntry>,
}

#[derive(Serialize, Deserialize)]
struct BackupEntry {
    rel_path: String,
    existed_before: bool,
    backup_name: Option<String>,     // existed_before=true 时才有
    original_sha256: Option<String>,
    deployed_sha256: String,
}
~~~

5. 还原：读 manifest，删除本次部署的文件，把 existed_before=true 的备份恢复回去，
   然后重新校验 sha256，最后删掉备份目录。
6. 写入用「临时文件 + 原子替换」，绝不允许出现半截文件。
7. 复制完成后删除目标文件的 :Zone.Identifier 备用流（上游文件是下载来的，带 MOTW）。
8. 部署后立即校验 sha256 与源一致，不一致自动回滚。
9. IO 路径禁止 unwrap/expect，统一 anyhow::Result。
10. 下载与复制放 std::thread，进度用 std::sync::mpsc 回传，UI 侧 ctx.request_repaint()。
    **不要引入 tokio。**

验收：
- 对空目录能完整部署并逐字节还原。
- 目标目录已有第三方 version.dll 时被拒绝并给出替代入口建议。
- 部署中途强杀进程，目录里不会留下半个文件。

---

## 2. 扫描模块

目标：列出本机 Steam / Epic 已安装游戏。

Steam：
- 注册表取根：HKCU\Software\Valve\Steam 的 SteamPath；兜底 HKLM\SOFTWARE\WOW6432Node\Valve\Steam 的 InstallPath。
- 库列表：<Steam>\steamapps\libraryfolders.vdf（嵌套结构 libraryfolders -> "0"/"1" -> path + apps）。
  **不要引入完整 VDF crate**，写约 80 行的最小解析（识别 "key" "value" { }），
  或直接扫各库的 steamapps\appmanifest_*.acf。
- 每个 appmanifest_*.acf 读 appid / name / installdir，游戏目录 = <lib>\steamapps\common\<installdir>。
- 过滤 Steamworks Redistributables 之类，只保留游戏且目录真实存在。

Epic：
- 清单目录 C:\ProgramData\Epic\EpicGamesLauncher\Data\Manifests\*.item（标准 JSON）。
  字段：DisplayName、InstallLocation、AppName、CatalogItemId、bIsApplication。
- 兜底注册表：HKLM\SOFTWARE\WOW6432Node\EpicGames\EpicGamesLauncher 的 AppDataPath。

统一输出：

~~~rust
#[derive(Clone, Copy, PartialEq)]
enum Launcher { Steam, Epic }

struct GameEntry {
    source: Launcher,
    name: String,
    app_id: String,
    install_dir: PathBuf,
    render_exe: Option<PathBuf>,
    deployed: DeployState,
    anticheat: AcVerdict,
}
~~~

约束：全流程只读，不写注册表、不改 VDF。扫描放后台线程并报进度，单个库解析失败只记录不 panic。
启动时不自动扫描，只在用户点「扫描」时执行（保证启动 < 1 s）。

验收：本机应列出 D:\Steam 下的游戏和 1 个 Epic 清单条目；目录不存在的条目置灰且不参与部署。

---

## 3. 反作弊模块

目标：部署前判定游戏目录 / 系统是否加载了内核级反作弊，命中则默认禁止部署。

三个数据源，全都不需要管理员权限：

1. 注册表服务项 HKLM\SYSTEM\CurrentControlSet\Services ，按名字匹配（本机 BEService 已命中）：
   EasyAntiCheat、EasyAntiCheat_EOS、BEDaisy、BEService、vgk、vgc、GameGuard、npggnt、
   XignCode、x3、PnkBstrA、PnkBstrB、Ricochet、Denuvo。
   同时读 ImagePath 与 Start（0=Boot、1=System、2=Auto），用于区分内核驱动与用户态服务。
2. 游戏目录特征文件：EasyAntiCheat\、EasyAntiCheat_EOS\、BattlEye\、BEService.exe、
   start_protected_game.exe、vgk.sys、GameGuard\、XignCode\，以及和游戏 EXE 同级的 *.sys。
3. 可选增强（需要管理员）：NtQuerySystemInformation(SystemModuleInformation) 枚举已加载内核模块，
   用来抓「已加载但服务名对不上」的驱动。拿不到就退回 1+2，**不要因此报错**。

判定与动作：

~~~rust
enum AcVerdict {
    None,
    UserMode(String),   // 黄色警告，允许部署
    Kernel(String),     // 红色，禁止部署
}
~~~

内核级命中时 UI 显示红色横幅并说明封号风险，默认不提供绕过按钮；
只在设置里加一个显式开关，且要求用户手动输入游戏名才放行。

约束：纯只读检测。绝不尝试加载 / 卸载 / 结束任何驱动或服务，不做注入。

验收：本机含 BattlEye 的游戏返回 Kernel；纯单机游戏返回 None。

---

## 4. 更新模块

**先纠正一个前提：这个仓库没有 GitHub Releases，也没有 Tags。**
实测 GET /repos/sdli1995/dlssg_for_sm86/releases -> [] ，/tags -> [] 。
文件是以提交形式直接放在 main 分支根目录的（version.dll 最近三次提交：
2026-09-07 init、2026-09-09 upgrade & optimize、2026-09-10 new fix version）。
所以「检查 Releases」这条需求在当前上游不成立，改成下面三条路：

版本探测（按成本排序）：
1. **内容指纹（推荐）**：GET /repos/sdli1995/dlssg_for_sm86/contents/version.dll
   返回 sha（git blob SHA-1）与 size。把上次的 sha 存本地 manifest，变了就是有更新。一次请求，不用下载。
2. **人类可读版本号**：GET raw README.md，首行是 "DLSSG Native 0.2.4"，
   正则 DLSSG Native (\d+\.\d+\.\d+) ；dlssg_sm86.ini 首行注释里也有同样的版本号。
3. **提交时间**：GET /repos/sdli1995/dlssg_for_sm86/commits?path=version.dll&per_page=1
   取 commit.committer.date。

下载地址（无 Release 资产）：
- https://raw.githubusercontent.com/sdli1995/dlssg_for_sm86/main/version.dll   （15.6 MB）
- https://raw.githubusercontent.com/sdli1995/dlssg_for_sm86/main/dlssg_sm86.ini （581 B）
- 回退源：archive/version.dll 是上一版快照。

完整性：
- 上游根目录**没有** .sha256 文件（README 提到的 .sha256 只存在于它对外发布的 ZIP 旁），
  所以不能依赖上游哈希。
- 用 TOFU：首次下载记录 sha256 到本地，后续每次下载都要求与记录不一致才算「新版本」。
- 更强的信号是 Authenticode：DLL 由自签证书 "DLSSG Native Project" 签名。
  用 WinVerifyTrust 校验「签名存在 + 签名者主体名匹配」。**证书链不受信任是预期状态，不能因此判失败**；
  签名者不匹配才拒绝安装。这能挡住被替换的二进制。
- 下载后删除 :Zone.Identifier 备用流。

约束：
- GitHub 未认证 API 限流 60 次/小时。只在用户点「检查更新」时请求，**不在启动时请求**；
  用 ETag / If-None-Match 缓存。
- 禁止后台轮询 —— 既是产品约束（关闭即退出）也是限流要求。
- 下载走后台线程 + mpsc 进度 + AtomicBool 取消。

验收：断网时友好报错不崩溃；模拟 blob sha 变化能正确提示有更新；签名者不符的 DLL 被拒装。

---

## 5. 打包模块

目标：产出单文件 exe + 安装包，安装包 < 10 MB。

1. 构建：cargo build --release --locked（release profile 见 Cargo.toml，已开 lto/opt-level=z/strip/panic=abort）。
2. **不要把 15.6 MB 的 version.dll 打进安装包**。它是运行时按需从上游下载的，
   这是能把安装包压到 10 MB 以下的关键。首次使用时提示用户下载。
3. 用 Inno Setup 做安装器：
   - 默认按用户安装到 %LOCALAPPDATA%\Programs\DLSSG-Manager ，不需要管理员权限。
   - 压缩用 lzma2/ultra64。
   - 写 Add/Remove Programs 项、开始菜单快捷方式、可选桌面快捷方式。
   - 卸载只删自己的文件，**不碰游戏目录**（游戏目录里的 DLL 由 App 内的还原功能处理）。
4. 版本信息与图标写进 exe（可用 winres build script）。
5. 不要用 UPX：加壳会显著提高杀软误报率，而这个工具本身已经在做 DLL 代理，误报本就敏感。
6. 可选：cargo-wix 出 MSI，或用 GitHub Actions 出 CI 产物。

验收：安装包 < 10 MB；安装后 exe 启动 < 1 s；任务管理器空闲内存 < 60 MB；关窗口后进程从任务管理器消失。

---

## 6. 总验收清单

- [ ] 空目录部署 -> 还原，逐字节一致
- [ ] 第三方 version.dll 冲突时不覆盖，给出替代入口
- [ ] 扫描能列出本机 D:\Steam 游戏 + Epic 清单游戏
- [ ] BattlEye 目录被判定为 Kernel 且禁止部署
- [ ] 上游 version.dll 变更后「检查更新」能识别
- [ ] 断网、GitHub 限流、文件被占用三种失败路径都有提示且不崩溃
- [ ] 安装包 < 10 MB、内存 < 60 MB、启动 < 1 s、关闭即退出
- [ ] 中文界面无方块（已内置系统字体加载逻辑）

---

## 7. 环境与构建实测事实（务必先读，能省掉几小时）

### 已装好的工具链（本机实测）
- rustc / cargo 1.98.1（x86_64-pc-windows-msvc，rustup 装在 C:\Users\Administrator\.cargo\bin）
- MSVC 14.44.35207 + Windows SDK 10.0.26100.0（VS 2022 BuildTools 路径：
  C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools）
- 已验证：cargo check 通过、cargo clippy 无告警、cargo build --release 成功

### 坑 1：egui 0.36 的 API 跟网上 99% 的教程完全不一样
0.36 是一次大改写（渲染换成 Vello，文本栈换成 skrifa + harfrust）。照抄旧例子必然编译不过。
写任何 UI 代码前，先读本机源码，不要凭记忆：

~~~text
C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\eframe-0.36.2\src\epi.rs
C:\Users\Administrator\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\egui-0.36.2\src\containers\panel.rs
~~~

具体差异：
- App trait 的方法是 fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame)，
  **不是** fn update(&mut self, ctx: &egui::Context, frame: &mut Frame)。另有可选的 fn logic(ctx, frame)。
- egui::TopBottomPanel 和 egui::SidePanel **已被删除**，统一成：
  egui::Panel::top("id").show(ui, |ui| { ... }) / Panel::bottom / Panel::left / Panel::right
- CentralPanel 还在，但 show 的签名变了：CentralPanel::default().show(ui, |ui| {...})，接的是 &mut Ui 而不是 &Context。
- 面板顺序有意义：Panel 必须先加，CentralPanel 必须最后加。
- FontDefinitions.font_data 是 BTreeMap<String, Arc<FontData>>（要 Arc::new 包一层），
  families 是 BTreeMap<FontFamily, Vec<String>>。FontData::from_owned(Vec<u8>)。
- AppCreator 返回 Result<Box<dyn App>, DynError>，所以是 Box::new(|cc| Ok(Box::new(App::new(cc))))。

### 坑 2：eframe 默认渲染后端是 wgpu，不是 glow
eframe 0.36 的 default features 含 wgpu，会拉进 wgpu-core / naga / ash / image，体积巨大。
Cargo.toml 必须写：

~~~toml
eframe = { version = "0.36", default-features = false, features = ["glow", "default_fonts"] }
~~~

注意：不能写 “winit/default” 这种带斜杠的 feature，那是 eframe 内部的 dep/feature 语法，
从外部指定会直接报 feature not allowed to contain slashes。幸好 glutin-winit 已经自己开了 winit/rwh_06，
glow 后端不需要 winit 的默认特性。

### 坑 3：reqwest 默认 TLS 会拉 aws-lc-sys，需要 CMake + NASM
reqwest 0.13 的 default -> default-tls -> rustls -> aws-lc-rs -> aws-lc-sys，这个 crate 要 CMake 和 NASM
才能编译，机器上都没有，会直接构建失败。Windows 上正确写法是走 schannel：

~~~toml
reqwest = { version = "0.13", default-features = false, features = ["blocking", "native-tls", "system-proxy"] }
~~~

### 实测的四个硬指标（release，本机 RTX + OpenGL）
| 指标 | 目标 | 实测 |
|---|---|---|
| exe 体积 | 安装包 < 10 MB | 5.12 MB（Inno Setup lzma 后大约 2–3 MB） |
| 启动到窗口 | < 1 s | 49 ms |
| 关闭即退出 | 是 | 是，CloseMainWindow 后进程干净退出 |
| 内存 | < 60 MB | 总工作集 94.8 MB / 私有工作集 64.1 MB ← **超标** |

内存这条要会读：总工作集里有约 30 MB 是共享的 nvoglv64.dll（NVIDIA OpenGL 驱动），
任何走 GPU 的 Windows 界面都躲不掉。评估自己的内存请看 WorkingSetPrivate。

### 内存瓶颈就是中文字体
- 加载 simhei.ttf（9.7 MB）会多占约 19 MB 私有内存。
- 带字体：WSPrivate 64.1 MB；用 DLSSG_NO_CJK_FONT=1 跳过字体：WSPrivate 44.8 MB。
- 试过关掉 eframe 的 default_fonts，只省 0.5 MB，不值得，已保留。
- 想把内存压到 60 MB 以下，唯一有效手段是**字体子集化**：
  用 build.rs 配 subsetter crate（fontations 出品，和 skrifa 同源）把 simhei 裁成只含 UI 用到的
  几百个字，产物约 200–400 KB，再 include_bytes! 进 exe。
  这样 WSPrivate 能回到 45 MB 左右，顺带也不再依赖系统装了哪个中文字体。
  本机没有可用的 Python（python.exe 是 Microsoft Store 占位符），所以走不了 pyftsubset。

### 诊断开关
设环境变量 DLSSG_NO_CJK_FONT=1 启动，会跳过中文字体加载（中文显示为方块），
用来量化字体到底占多少内存。

### 坑 4：不要用 PE 导入表去认「哪个 exe 是渲染器」——实测不成立
本文档早先版本建议「读 PE 导入表找导入 d3d12.dll 的 exe」。实机测下来这条路走不通：

~~~text
cs2.exe       (2.9 MB)  -> 只静态导入 user32.dll, kernel32.dll
vconsole2.exe (5.0 MB)  -> 导入 user32, tier0, qt5core, qt5gui, qt5widgets, ws2_32, steam_api64, kernel32
TslGame.exe   (232 MB)  -> 导入表不在文件前 16 MB 内（读不到）
~~~

游戏普遍动态加载 D3D12，或者导入表在文件很后面。**更糟的是**：只看「父目录叫 Win64」
会把 PUBG 的 \Engine\Binaries\Win64\UnrealCEFSubProcess 和 CS2 的
\game\bin\win64\vconsole2 当成正主。

最终采用的方案是**多信号加权 + 排除表**（src/scan.rs 的 find_render_exe）：
- +50 文件名以 -Win64-Shipping.exe 结尾
- +30 符合 UE 约定 <Project>\Binaries\Win64\<Project>.exe
- +20 exe 名与游戏安装目录名一致
- +10 父目录叫 Win64
- +15 体积 > 10 MB
- 命中排除表（unrealcefsubprocess / vconsole / cef / crashreport / launcher ...）直接丢弃
- 最高分 <= 0 就返回 None，让用户手动选，**不要猜**

本机四款游戏实测全部命中：CS2 -> cs2.exe、PUBG -> TslGame.exe、
霍格沃茨之遗 -> HogwartsLegacy.exe、Wallpaper Engine -> wallpaperui.exe。

### 坑 5：altnative 下的四个 DLL 不是 version.dll 改个名
它们是导出名不同的独立二进制，体积都不一样（15666496 / 15668032 / 15674176 /
15678272，对比默认的 15667520）。选了非默认入口必须下载对应那个文件，
不能拿 version.dll 重命名，否则游戏不会加载它。

### 可复现的自测命令
~~~powershell
cargo run -- --selftest        # 扫描游戏库 + BEService 检出 + 上游 6 个资产 blob sha
cargo run -- --deploytest      # 部署/备份/还原 端到端断言
cargo run -- --downloadtest    # 下载 ini 并验证 git blob sha 一致
cargo run -- --pedump <exe>    # 打印 PE 导入表
~~~

