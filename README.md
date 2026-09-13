# FrameGen Manager

轻量 Windows 桌面工具，把 [sdli1995/dlssg_for_sm86](https://github.com/sdli1995/dlssg_for_sm86)
的代理 DLL + INI 部署到游戏渲染 EXE 所在目录，让 RTX 30 系（SM86）/ 20 系（SM75）启用 DLSS 帧生成。

Rust + egui/eframe。无 Electron / Tauri / WebView。

## 下载和使用

到 Releases 页下载 **FrameGen-Manager-v*-win64.zip**，解压后双击里面的
framegen-manager.exe 即可，**不需要安装，也不需要装任何运行库**。

然后用三步：

1. 点「扫描 Steam / Epic」找到你的游戏，点「用作部署目录」
   （或手动点「选择目录...」选到游戏渲染 EXE 所在文件夹）
2. 在「上游资产」卡片点「下载 / 更新资产」
3. 在「部署」卡片点「部署」

进游戏后在画面设置里打开 DLSS 帧生成。详细说明和常见问题见发布包里的「使用说明.txt」。

**注意**：程序没有买代码签名证书，Windows 可能弹「已保护你的电脑」——
点「更多信息」→「仍要运行」。这是所有未签名软件的通病。

## 部署到游戏目录的 4 个文件

| 文件 | 大小 | 来源 |
|---|---|---|
| version.dll（或 altnative 里的替代入口） | 15.6 MB | sdli1995/dlssg_for_sm86 |
| dlssg_sm86.ini | 581 B | 同上（按显卡自动改写 Router） |
| nvngx_dlssg.dll | 7.1 MB | 见下方「DLSS 运行库来源」 |
| nvngx_dlss.dll | 56.2 MB | 见下方「DLSS 运行库来源」 |

全部部署到游戏**实际渲染 EXE 所在目录**（通常是 ...\Binaries\Win64\）。

## DLSS 运行库来源（重要）

很多游戏目录里没有 nvngx_dlssg.dll / nvngx_dlss.dll，只有前面两个文件是不生效的，
所以本工具会一并下载并部署这两个运行库。

- 版本：DLSS FG 310.9.1、DLSS SR 310.9.1
- 获取方式：从社区仓库 **[RankFTW/rhi-repo](https://github.com/RankFTW/rhi-repo)** 的
  GitHub Releases 取对应的 zip，解压出里面的 DLL
- 为什么可以信任：这两个 DLL **由 NVIDIA 官方签名**（签名状态 Valid，签名者
  CN=NVIDIA Corporation）。本工具在解压后会读取 PE 证书表校验签名者必须是 NVIDIA，
  不符合就丢弃并报错。该仓库只是把官方文件按版本搬运打包，没有改动内容。
- 解压完成后会**删掉下载的 zip**，只保留解压出来的 DLL。

本工具与 NVIDIA、sdli1995、RankFTW 均无关联。运行库版权归 NVIDIA。

## 关于上游的两个重要事实

1. **上游没有 GitHub Releases，也没有 Tags。** 文件直接提交在 main 分支根目录。
   所以更新检查走 contents API 的 git blob sha 做变更指纹，即
   sha1("blob <len>\0" + content)；下载后本地重算这个哈希并与 API 返回值比对。
2. **altnative/ 下的四个 DLL 不是 version.dll 改个名**，而是导出名不同的独立二进制
   （体积都不同）。选了非默认入口就必须下载对应那一个。

## 游戏图标

扫描游戏库时，会从每个游戏的**渲染 EXE** 里提取图标显示在列表里。
走的是 Windows Shell API，所以不管游戏来自 Steam、Epic 还是手动添加都能取到。

## 自动判断代理入口

五个入口（version / winmm / dinput8 / winhttp / dxgi）是同一个 Mod，选错只会不生效，
不会损坏游戏。判断依据不是游戏名字，而是**导入表**：Windows 加载 DLL 时优先搜索 EXE
所在目录，所以只要游戏会加载的某个模块导入了 version.dll，把代理放进去就会被加载。

判定顺序：

1. 目录里已有本项目的文件 -> 直接复用那个入口（避免同时存在两个代理）
2. 排除被第三方占用的入口
3. 按导入表匹配，优先级 version -> winmm -> dinput8 -> winhttp -> dxgi
4. 都判不出来就用上游默认 version.dll，并在界面上明确说明「未能自动判定」

## 识别「用户是否已经手动装过本项目」

本项目的 5 个 DLL 都用 **DLSSG Native Project** 自签证书签名，这是比文件名或体积
可靠得多的身份标记。工具会读取目标目录里已有文件的 PE 证书表：

- 签名者是 DLSSG Native Project -> 本项目的文件，**允许安全覆盖**（不再误拒）
- 其他签名者 / 无签名 -> 第三方文件，**拒绝覆盖**（代理入口会跳过并换一个）

## 下载与备用源

官方下载走 raw.githubusercontent.com。国内可能连不上，界面在**下载失败后**会提示
改用备用源，用户填前缀即可。内置建议值：https://ghproxy.net/

备用源返回的内容同样会用 git blob sha 校验，内容不符会被拒绝，所以换镜像不影响安全性。

## 构建

    cargo run --release        # 直接跑
    cargo build --release      # 产物 target\release\dlssg-manager.exe

依赖的坑（已在 Cargo.toml 里规避）：
- eframe 默认渲染后端是 wgpu，必须 default-features = false 且只开 glow + default_fonts
- reqwest 默认 TLS 会拉 aws-lc-sys（需要 CMake + NASM），必须改用 native-tls
- 加了 flate2 用于解压运行库的 zip。它本来就在依赖树里（png -> image -> eframe），
  所以是零新增下载、零体积增加
- .cargo/config.toml 开了 +crt-static，去掉对 vcruntime140.dll 的依赖，
  这样用户不装 VC++ 运行库也能直接跑

## 自测命令

    cargo run -- --selftest        # 扫描游戏库 + 反作弊 + 更新检查 + 入口推荐 + 显卡
    cargo run -- --deploytest      # 部署 / 备份 / 还原 / 冲突 / 已装过本项目 端到端断言
    cargo run -- --downloadtest    # 下载 581B 的 ini 并验证 git blob sha
    cargo run -- --canceltest      # 取消下载 + 残留清理
    cargo run -- --dlssrun         # 下载 DLSS 运行库：查 release -> 下载 -> 解压 -> 删包 -> 验签
    cargo run -- --ziptest <zip> <输出>   # 单独测 zip 解压
    cargo run -- --idtest <文件>          # 看一个文件的签名身份
    cargo run -- --pedump <exe>           # 打印 PE 导入表
    cargo run -- --icontest <exe>         # 提取图标并打印尺寸/透明度统计

## 打包

用 Inno Setup 6：

    iscc packaging\installer.iss

安装包刻意不包含那些大文件 —— 运行时在 App 内点「下载 / 更新资产」获取，
这样安装包才能压到 10 MB 以下。卸载不会碰游戏目录。

## 实测指标（release，本机）

| 指标 | 目标 | 实测 |
|---|---|---|
| exe 体积 | 安装包 < 10 MB | 6.27 MB |
| exe 运行库依赖 | 不要求用户装 VC++ | 17 个系统 DLL，无 vcruntime140.dll |
| 启动 | < 1 s | 约 160 ms |
| 关闭即退出 | 是 | 是 |
| 内存 | < 60 MB | 私有工作集约 64 MB，见下 |

内存超出 60 MB 的部分几乎全部来自中文字体：加载 simhei.ttf（9.7 MB）会多占约 19 MB
私有内存。要达标需要做字体子集化（build.rs + fontsubset）。
设 DLSSG_NO_CJK_FONT=1 可跳过字体加载，用来量化这部分开销。

## 资产与配置文件位置

- 资产默认放在 **exe 同级目录的 assets\\**（便携，解压即用）；该目录不可写时
  回退到 %APPDATA%\FrameGen-Manager\assets\
- 用户可以在界面上更改资产目录（会记到配置文件里）
- 备份始终放在 %APPDATA%\FrameGen-Manager\backups\ —— 存的是游戏原始文件的副本，
  丢了就没法还原，所以不跟着 exe 走
- 配置文件：优先 exe 同级 framegen-manager.json，不可写则 %APPDATA%\FrameGen-Manager\config.json
