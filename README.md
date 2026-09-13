# FrameGen Manager

给 RTX 20 / 30 系显卡的游戏**一键开启 DLSS 帧生成**。

Windows 桌面工具，把 [sdli1995/dlssg_for_sm86](https://github.com/sdli1995/dlssg_for_sm86)
的代理 DLL + 配置部署到游戏渲染 EXE 所在目录，并自动处理 DLSS 运行库、显卡路由和入口冲突。

## 下载和使用

到 [Releases](../../releases) 下载 zip，解压后双击 framegen-manager.exe。
**不需要安装，也不需要装任何运行库。**

三步：

1. 点「扫描 Steam / Epic」找到你的游戏，点「用作部署目录」
   （也可以手动点「选择目录...」选到游戏渲染 EXE 所在文件夹）
2. 在「上游资产」卡片点「下载 / 更新资产」
3. 在「部署」卡片点「部署」

进游戏后在画面设置里打开 DLSS 帧生成。更详细的说明和常见问题见发布包里的「使用说明.txt」。

> **注意**：程序没有买代码签名证书，Windows 可能弹「已保护你的电脑」。
> 点「更多信息」→「仍要运行」即可，这是所有未签名软件的通病。

## 会往游戏目录放什么

| 文件 | 用途 |
|---|---|
| version.dll | 代理入口（名字被占用时自动改用 winmm / dinput8 / winhttp / dxgi） |
| dlssg_sm86.ini | 配置（RTX 20 系会自动改 Router=SM75） |
| nvngx_dlssg.dll | DLSS 帧生成运行库 |
| nvngx_dlss.dll | DLSS 超分运行库 |

全部放在游戏**实际渲染 EXE 所在目录**（通常是 ...\Binaries\Win64\），合计约 79 MB。

部署前会先备份被覆盖的原文件，随时可以在「部署」卡片点「还原」把游戏目录恢复原样。

## 什么情况下不要用

- 游戏有**内核级反作弊**（EAC / BattlEye / Vanguard 等）
  —— 程序会检测到并直接阻止部署，改游戏文件有封号风险
- 显卡是 **RTX 40 / 50 系** —— 原生就支持帧生成，不需要本工具
- 不是 NVIDIA 显卡

## 关于 DLSS 运行库的来源

很多游戏目录里没有 nvngx_dlssg.dll / nvngx_dlss.dll，只放前两个文件是不生效的，
所以本工具会一并下载部署这两个运行库（DLSS FG 310.9.1 / DLSS SR 310.9.1）。

- 来源：社区仓库 [RankFTW/rhi-repo](https://github.com/RankFTW/rhi-repo) 的 GitHub Releases
- **为什么可以信任**：这两个 DLL 由 **NVIDIA 官方签名**。程序解压后会读取 PE 证书表，
  校验签名者必须是 NVIDIA Corporation，不符合就丢弃报错。该仓库只做搬运打包，没有改动内容。
- 下载完的 zip 会**立即删除**，只保留解压出来的 DLL。

**本程序不分发、也不打包 NVIDIA 的任何文件。** 运行库由用户在程序内主动触发下载，
版权归 NVIDIA Corporation 所有。本工具与 NVIDIA、sdli1995、RankFTW 均无关联。

## 数据放在哪

- **程序同级的 assets 文件夹** —— 下载的资产（约 79 MB）。
  想做成绿色便携版，整个文件夹拷走就行
- **%APPDATA%\FrameGen-Manager** —— 部署备份和设置。
  备份故意不跟 exe 走，免得误删程序文件夹后没法还原游戏
- 资产目录可以在界面上改成别的位置

## 许可

[MIT](LICENSE)。第三方组件许可见 [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt)。

开发相关的说明（构建、测试、打包、实现原理）见 [DEVELOPING.md](DEVELOPING.md)。
