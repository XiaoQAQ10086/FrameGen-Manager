; FrameGen Manager - Inno Setup 6 打包脚本
; 编译： iscc packaging\installer.iss
;
; 设计要点：
;  1. 不把那些大文件（version.dll、nvngx_dlssg.dll、nvngx_dlss.dll）打进安装包，
;     它们是运行时按需下载的。这是能把安装包压到 10 MB 以下的关键。
;     这是能把安装包压到 10 MB 以下的关键。
;  2. 默认按用户安装到 %LOCALAPPDATA%\Programs，不需要管理员权限。
;  3. 卸载只删程序自己的文件，绝不碰游戏目录（游戏目录里的文件由 App 内的「还原」处理）。
;  4. 不使用 UPX：加壳会显著提高杀软误报率，而这个工具本身就在做 DLL 代理，对误报很敏感。

#define AppName "FrameGen Manager"
; 版本号可由打包脚本传入：iscc /DAppVersion=1.2.3 installer.iss
#ifndef AppVersion
#define AppVersion "0.9.6"
#endif
#define AppExeName "framegen-manager.exe"

[Setup]
AppId={{7C4B1E2A-9D53-4A6E-8F1C-FRAMEGENMANAGER01}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=commandline
OutputDir=..\dist
OutputBaseFilename=FrameGen-Manager-{#AppVersion}-setup
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayName={#AppName}
; 若准备了图标，取消下面两行注释
; SetupIconFile=app.ico
; UninstallDisplayIcon={app}\{#AppExeName}

[Languages]
Name: "chinese"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; Flags: unchecked

[Files]
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "README.md"; DestDir: "{app}"; Flags: ignoreversion
; 注意：这里刻意不包含 version.dll / dlssg_sm86.ini。
; 它们在 App 内点「下载 / 更新资产」时从上游仓库获取，并做 git blob sha 校验。

[Icons]
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "启动 {#AppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; 只清理程序自己的安装目录
Type: filesandordirs; Name: "{app}"

[Code]
// 卸载时提醒：备份数据保留在 %APPDATA%\FrameGen-Manager，
// 如果还有游戏处于「已部署」状态，应该先在 App 里还原再卸载。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    if MsgBox('程序已卸载。' + #13#10 + #13#10 +
              '注意：%APPDATA%\FrameGen-Manager 下的部署备份和已下载资产被保留。' + #13#10 +
              '如果还有游戏里存在本工具部署的 DLL，请重新安装后用「还原」功能清理。' + #13#10 + #13#10 +
              '是否现在打开该目录？', mbConfirmation, MB_YESNO) = IDYES then
    begin
      ShellExec('open', ExpandConstant('{userappdata}\FrameGen-Manager'), '', '', SW_SHOWNORMAL, ewNoWait, ErrorCode);
    end;
  end;
end;
