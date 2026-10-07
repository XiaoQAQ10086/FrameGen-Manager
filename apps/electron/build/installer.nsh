; ============================================================================
; FrameGen Manager - electron-builder NSIS hooks
;
; Two jobs, both about upgrading from the OLD (Inno Setup) builds:
;
;   1. customInit: the old app's self-update runs the installer with Inno Setup
;      flags (/SILENT /NORESTART /SUPPRESSMSGBOXES /DIR=<dir>). NSIS knows /S and
;      /D= only, so translate them - otherwise the upgrade pops the full wizard
;      and installs into the default directory (the user's settings, game library
;      and downloaded assets live in the old directory and would look lost).
;
;   2. customInstall: relaunch the app after a SILENT install. electron-builder
;      only auto-runs the app from the finish page, which silent installs skip.
;
; ASCII only on purpose: NSIS reads scripts with the current code page.
; ============================================================================

; StrFunc/FileFunc are only needed by the installer pass. Declaring them for the
; uninstaller pass too makes makensis warn (6010: install function not referenced)
; and electron-builder treats warnings as errors.
!ifndef BUILD_UNINSTALLER
  !include "FileFunc.nsh"
  !include "StrFunc.nsh"
  ${StrLoc}
!endif

; AppId of the old Inno Setup installer (packaging/installer.iss).
!define FGM_OLD_UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\{7C4B1E2A-9D53-4A6E-8F1C-5B2D9E4A77C1}_is1"

!macro customInit
  ; ---- 1) translate the old Inno-style command line ----
  ${GetParameters} $R9
  ${StrLoc} $R0 $R9 "/SILENT" ">"
  ${If} $R0 != ""
    SetSilent silent
  ${EndIf}
  ${StrLoc} $R0 $R9 "/DIR=" ">"
  ${If} $R0 != ""
    IntOp $R0 $R0 + 5
    StrCpy $R1 $R9 "" $R0
    StrCpy $R2 $R1 1
    ${If} $R2 == '"'
      StrCpy $R1 $R1 "" 1
    ${EndIf}
    ; stop at the closing quote, or at the first space when unquoted
    ${StrLoc} $R3 $R1 '"' ">"
    ${If} $R3 == ""
      ${StrLoc} $R3 $R1 " " ">"
    ${EndIf}
    ${If} $R3 != ""
      StrCpy $R1 $R1 $R3
    ${EndIf}
    ${If} $R1 != ""
      StrCpy $INSTDIR $R1
      ; ---- 光设 $INSTDIR 不够（实测踩到）----
      ; electron-builder 的多用户宏是在 customInit **之后**才跑的：它用注册表里的
      ; InstallLocation 覆盖 $INSTDIR（multiUser.nsh），最后才让 NSIS 自己的 /D= 覆盖。
      ; 于是旧版传 /DIR=D:\某处，新包装完仍落在 C:\Program Files（实测 exit 0、
      ; 文件确实没进指定目录）—— NSIS 只认自己的 /D=，不认 Inno 的 /DIR=。
      ; 所以把翻译出来的目录**写进那个宏真正会读的键**（Software\<APP_GUID>）；
      ; 两个 hive 都写：提权安装读 HKLM，按用户安装读 HKCU，哪个成功都行。
      !ifdef APP_GUID
        ClearErrors
        WriteRegStr HKCU "Software\${APP_GUID}" "InstallLocation" "$R1"
        ClearErrors
        WriteRegStr HKLM "Software\${APP_GUID}" "InstallLocation" "$R1"
        ClearErrors
      !endif
    ${EndIf}
  ${EndIf}

  ; ---- 2) remove the old Inno Setup installation's program files ----
  ; We do NOT run its uninstaller: the MsgBox in its [Code] section is not
  ; suppressed by /SUPPRESSMSGBOXES and would hang a silent update (seen in
  ; the Tauri build). User data (framegen-manager.json, game_library.json,
  ; assets, backups, logs) is never touched.
  ReadRegStr $R5 HKCU "${FGM_OLD_UNINSTKEY}" "UninstallString"
  ${If} $R5 != ""
    nsExec::Exec 'taskkill /F /IM framegen-manager.exe'
    Pop $R6
    Delete "$INSTDIR\framegen-manager.exe"
    Delete "$INSTDIR\unins000.exe"
    Delete "$INSTDIR\unins000.dat"
    DeleteRegKey HKCU "${FGM_OLD_UNINSTKEY}"
  ${EndIf}

  ; ---- 3) installed copy: move data out before the old uninstaller wipes INSTDIR ----
  IfFileExists "$INSTDIR\installed.txt" 0 fgm_no_data_move
    !insertmacro FGM_MOVE_DATA_OUT
  fgm_no_data_move:
!macroend

!macro customInstall
  ; Mark this copy as INSTALLED. The main process keys on this file to put user data
  ; in %APPDATA%\FrameGen-Manager instead of next to the exe: electron-builder's
  ; uninstaller ends with "RMDir /r $INSTDIR" and the install flow runs the
  ; previous version's uninstaller first, so data kept in the install directory is
  ; wiped on every update (measured: config + game library + 95 MB of assets gone
  ; on the second install). Portable copies have no marker and keep the old layout.
  ClearErrors
  FileOpen $9 "$INSTDIR\installed.txt" w
  IfErrors +3
    FileWrite $9 "installed"
    FileClose $9

  ; 安装目录里**不再放**「使用说明.txt」（用户要求删除）—— 安装包本身已经不带了，
  ; 新装用户不会有这个文件。
  ; 升级用户那份旧文件由应用自己清（main.js 的 dropLegacyDocs，便携版能删掉；
  ; 装在 Program Files 时应用没有写权限，删不掉就留着，不影响任何功能）。
  ; 这里**故意不做**：试过 nsExec + cmd 通配（单引号/双引号、for 循环、???? 通配都试了），
  ; 命令手工执行能删、在安装器里始终没生效，与其留一条假装在干活的语句，不如不写。

  ${If} ${Silent}
    ; silent update: bring the new version up (otherwise the user ends up
    ; with no window at all after the update)
    Exec '"$INSTDIR\${APP_EXECUTABLE_FILENAME}"'
  ${EndIf}
!macroend

; Move user data out of the install directory and into %APPDATA%\FrameGen-Manager.
;
; MUST run in customInit (.onInit): right after this, electron-builder's install
; flow runs the PREVIOUS version's uninstaller, whose last statement is
; 'RMDir /r $INSTDIR'. Anything still sitting in the install directory at that
; moment is destroyed (measured: config + game library + 95 MB of assets gone
; when installing the same package twice).
;
; Only for INSTALLED copies (installed.txt marker); portable copies keep their
; data next to the exe and never run this path.
!macro FGM_MOVE_DATA_OUT
  StrCpy $R8 "$APPDATA\FrameGen-Manager"
  ; already migrated once? then AppData is authoritative
  IfFileExists "$R8\framegen-manager.json" fgm_move_done
  CreateDirectory "$R8"
  IfFileExists "$INSTDIR\framegen-manager.json" 0 +2
    CopyFiles /SILENT "$INSTDIR\framegen-manager.json" "$R8"
  IfFileExists "$INSTDIR\game_library.json" 0 +2
    CopyFiles /SILENT "$INSTDIR\game_library.json" "$R8"
  IfFileExists "$INSTDIR\tips.md" 0 +2
    CopyFiles /SILENT "$INSTDIR\tips.md" "$R8"
  IfFileExists "$INSTDIR\assets\*.*" 0 +3
    nsExec::Exec 'xcopy /E /I /Y /Q "$INSTDIR\assets" "$R8\assets\"'
    Pop $0
  IfFileExists "$INSTDIR\backups\*.*" 0 +3
    nsExec::Exec 'xcopy /E /I /Y /Q "$INSTDIR\backups" "$R8\backups\"'
    Pop $0
  IfFileExists "$INSTDIR\logs\*.*" 0 +3
    nsExec::Exec 'xcopy /E /I /Y /Q "$INSTDIR\logs" "$R8\logs\"'
    Pop $0
  fgm_move_done:
!macroend

; ---------------------------------------------------------------- 卸载
; 用户主动卸载时把数据一起清干净（%APPDATA%\FrameGen-Manager：设置、游戏库、资产、备份、日志）。
; 关键：**静默卸载要保留数据** —— 装新版时会先跑旧版的卸载器（静默），那一步清了数据，
; 就等于每次更新都把用户的设置和资产清空（我们踩过这个坑，见 installSection.nsh 的注释）。
; electron-builder 自己的卸载器负责装目录、开始菜单项和它自己的注册表键；这里只补数据目录。
!macro customUnInstall
  IfSilent fgm_uninst_keep
    RMDir /r "$APPDATA\FrameGen-Manager"
    ; 旧版 Inno 的卸载键（正常卸载时一并清掉，免得多出一条幽灵条目）
    DeleteRegKey HKCU "${FGM_OLD_UNINSTKEY}"
  fgm_uninst_keep:
!macroend
