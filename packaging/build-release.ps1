# FrameGen Manager —— 发布打包脚本（Electron 版）
#
# 用法：  pwsh -ExecutionPolicy Bypass -File packaging\build-release.ps1 [-Node <node.exe>]
#
# 产出（全部落在 dist\）：
#   dist\FrameGen-Manager-v<版本>-setup.exe   中文 NSIS 安装包 —— 这是唯一产物
#   dist\SHA256SUMS.txt                       校验和（只有安装包那一行）
#
# 只发安装版（用户要求）：原来还会组装一份便携 zip（win-unpacked 打包），现在不做 ——
# 便携版没有 installed.txt 标记、数据会落在程序目录，"绿色版"那条路已经不再发布。
#
# 自更新按文件名找安装包：必须是 FrameGen-Manager-v<版本>-setup.exe。
# electron-builder 的 artifactName 已经产出这个名字，这里只是复制进来。
#
# 历史：0.9.19 及更早是 egui 版（Inno Setup 打包），Tauri 版做过一版但没发布。
# 两代都已经退役，本脚本只出 Electron 产物 —— 不再是「谁在就发谁」。

param(
    # Node 可执行文件（electron-builder 是 npm 包，必须由 node 跑）。
    # 默认从 PATH 找；找不到就报错退出（没有 Node 就出不了 Electron 包）。
    [string]$Node = ''
)

$ErrorActionPreference = 'Stop'

if (-not $Node) {
  $cmd = Get-Command node -ErrorAction SilentlyContinue
  if ($cmd) { $Node = $cmd.Source }
  elseif (Test-Path 'C:\Program Files\nodejs\node.exe') { $Node = 'C:\Program Files\nodejs\node.exe' }
}
if (-not $Node) {
  throw ' 找不到 node（electron-builder 需要它）。装了 Node 仍找不到时用 -Node <node.exe 路径> 指定。'
}

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 版本号从 crates\core\Cargo.toml 读 —— 那是 core 的 SELF_VERSION 来源，也是唯一真源。
# （旧版从根 Cargo.toml 读；egui 根包已删，根清单现在只是工作区。）
$m = Select-String -Path 'crates\core\Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { throw 'crates\core\Cargo.toml 里找不到 version' }
$version = $m.Matches[0].Groups[1].Value
# 再校验根清单里给自更新用的那一行（[workspace.package] version）与它一致
$m2 = Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m2) { throw 'Cargo.toml 里找不到 version（[workspace.package] 那一段没了？自更新会读不到版本号）' }
$versionSelf = $m2.Matches[0].Groups[1].Value
if ($versionSelf -ne $version) { throw ("版本号不一致：crates\core\Cargo.toml=" + $version + " 但 Cargo.toml=" + $versionSelf) }
$stageName = "FrameGen-Manager-v$version"
Write-Output "版本 = $version"

Write-Output '[1/5] 版本号一致性检查 ...'
# 两处必须一致：根 Cargo.toml（core 的 SELF_VERSION 源头）与
# apps\electron\package.json（Electron 版 —— 安装包与自更新都按它拼文件名）。
# 对不上的后果：自更新下载到错的版本，或者干脆找不到文件。
# （apps\desktop 那套已退役的 Tauri 界面已删除，不再参与检查。）
$checks = @(
  @{ Path = 'apps\electron\package.json';   Pattern = '"version"\s*:\s*"([^"]+)"' }
)
foreach ($c in $checks) {
  $f = Join-Path $root $c.Path
  if (-not (Test-Path $f)) { continue }
  $mm = Select-String -Path $f -Pattern $c.Pattern | Select-Object -First 1
  if (-not $mm) { throw ('在 ' + $c.Path + ' 里找不到 version') }
  $v = $mm.Matches[0].Groups[1].Value
  if ($v -ne $version) { throw ('版本不一致：根 Cargo.toml=' + $version + '，' + $c.Path + '=' + $v) }
}
Write-Output ('  版本号一致：' + $version)

Write-Output '[2/5] sidecar 冒烟测试（不过就不许打包）...'
# 为什么要有这一步：命令层已经从「Rust 直接跑」改成「Electron -> sidecar -> core」，
# 出包前必须确认 sidecar 起得来、能回 JSON。挂在这里比让用户撞上「后台进程没起来」好。
$sc = Join-Path $root 'apps\sidecar\target\release\framegen-sidecar.exe'
if (-not (Test-Path $sc)) {
  throw (' 没找到 sidecar：' + $sc + '。先在 apps\sidecar 里跑 cargo build --release。')
}
$smoke = Join-Path $env:TEMP ('fgm-smoke-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Force -Path $smoke | Out-Null
$prevDataDir = $env:FGM_DATA_DIR
$prevConsoleEnc = [Console]::OutputEncoding
# sidecar 的 JSON 是 UTF-8；不强制控制台编码时，中文标签会被按 OEM 解码，
# 可能把引号/逗号吞掉，ConvertFrom-Json 直接报 unexpected character。
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new()
$env:FGM_DATA_DIR = $smoke
try {
  $reqs = @('{"id":1,"cmd":"ping"}', '{"id":2,"cmd":"gpu_info"}', '{"id":3,"cmd":"get_config"}') -join [Environment]::NewLine
  $out = @($reqs | & $sc 2>$null)
  if ($out.Count -lt 3) { throw (' sidecar 只回了 ' + $out.Count + ' 条响应，应该 3 条') }
  $objs = @()
  foreach ($line in $out) {
    $o = $line | ConvertFrom-Json
    if (-not $o.ok) { throw (' sidecar 命令失败：' + $o.error) }
    $objs += $o
  }
  Write-Output ('  ping / gpu_info / get_config 都通过（' + $out.Count + ' 条响应）')
  # 自报版本必须等于本次要发的版本号 —— 这一条是踩过坑才加的：
  # cargo 按**源码**变化决定重建，只改 Cargo.toml 里的版本号不一定触发 framegen-core 重新编译，
  # 于是侧车里的 SELF_VERSION 还是上一版；发出去以后程序内更新检查会把新版判成旧版，
  # 用户装完又被提示更新到同一版（死循环）。实测 1.0.3 的侧车自报 1.0.2。
  $vc = ($objs | Where-Object { $_.id -eq 3 }).result.version
  if ($vc -ne $version) {
    throw (' sidecar 自报版本 ' + $vc + ' != 本次要发的 ' + $version + '。先 cargo clean -p framegen-core，再重新 cargo build --release。')
  }
  Write-Output ('  sidecar 自报版本与要发的一致：' + $vc)
} finally {
  $env:FGM_DATA_DIR = $prevDataDir
  [Console]::OutputEncoding = $prevConsoleEnc
  Remove-Item $smoke -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Output '[2b/5] 七道前端校验（不绿不许出包）...'
$dev = Join-Path (Split-Path -Parent (Split-Path -Parent $PSScriptRoot)) 'dev-only'
if (Test-Path (Join-Path $dev 'i18n-gen.py')) {
  $env:PYTHONIOENCODING = 'utf-8'
  $env:NODE_PATH = 'C:/Users/Administrator/.workbuddy/binaries/node/workspace/node_modules'
  $py = 'C:/Users/Administrator/.workbuddy/binaries/python/versions/3.13.12/python.exe'
  if (-not (Test-Path $py)) { throw "七道校验需要 python：$py 不存在" }
  $fail = 0
  foreach ($f in @('apps\electron\ui\app.js','apps\electron\ui\i18n.js','apps\electron\main.js','apps\electron\preload.js')) {
    & $Node --check (Join-Path $root $f); if ($LASTEXITCODE) { $fail++; Write-Output "  语法不过：$f" }
  }
  & $Node (Join-Path $dev 'i18n-check.js'); if ($LASTEXITCODE) { $fail++ }
  & $py (Join-Path $dev 'i18n-coverage.py'); if ($LASTEXITCODE) { $fail++ }
  & $py (Join-Path $dev 'i18n-html-audit.py'); if ($LASTEXITCODE) { $fail++ }
  & $Node (Join-Path $dev 'ui-e2e\check-t-shadow.js'); if ($LASTEXITCODE) { $fail++ }
  & $Node (Join-Path $dev 'ui-e2e\run.js'); if ($LASTEXITCODE) { $fail++ }
  & $Node (Join-Path $root 'packaging\ipc-contract-check.js'); if ($LASTEXITCODE) { $fail++ }
  & $py (Join-Path $dev 'i18n-sync.py'); if ($LASTEXITCODE) { $fail++ }
  if ($fail) { throw "七道校验有 $fail 项不通过，中止打包" }
  Write-Output '  七道校验全部通过'
}

Write-Output '[3/5] 把后台进程按软件名放好，再交给 electron-builder ...'
# 任务管理器里显示的是**文件名**：后台进程原来叫 framegen-sidecar.exe，用户看到一条看不懂的
# 进程。这里把它复制成「FrameGen Manager Sidecar.exe」，主程序也按这个名字找它。
# 先复制再打包（electron-builder 的 extraFiles 只做拷贝，不能顺手改名）。
$scSrc = Join-Path $root 'apps\sidecar\target\release\framegen-sidecar.exe'
$scDst = Join-Path $root 'apps\electron\FrameGen Manager Sidecar.exe'
if (-not (Test-Path $scSrc)) { throw (' 没找到 sidecar：' + $scSrc) }
Copy-Item $scSrc $scDst -Force
Write-Output ('  ' + (Split-Path $scDst -Leaf))

Write-Output '[3/5] electron-builder 打包（NSIS 安装包）...'
$ebDir = Join-Path $root 'apps\electron'
# electron-builder 自己的二进制走国内镜像：不走镜像实测只有 67 KB/s，走镜像 24 秒
$env:ELECTRON_BUILDER_BINARIES_MIRROR = 'https://npmmirror.com/mirrors/electron-builder-binaries/'
$env:ELECTRON_MIRROR = 'https://npmmirror.com/mirrors/electron/'
$prevEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
Push-Location $ebDir
try {
  & $Node 'node_modules\electron-builder\out\cli\cli.js' --win nsis --x64 2>&1 |
    Select-Object -Last 3 | ForEach-Object { Write-Output ('    ' + $_) }
  $ebOk = ($LASTEXITCODE -eq 0)
} finally { Pop-Location }
$ErrorActionPreference = $prevEap
if (-not $ebOk) { throw ' electron-builder 失败，中止发布' }

$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$setup = Join-Path $dist "$stageName-setup.exe"
$built = Join-Path $root ('dist-electron\' + $stageName + '-setup.exe')
if (-not (Test-Path $built)) { throw (' 没找到 electron-builder 的安装包产物：' + $built) }
Copy-Item $built $setup -Force
Write-Output ('  ' + $stageName + '-setup.exe')

# 注：安装目录里**不再放**「使用说明.txt」（用户要求删除）。
# 使用说明以界面内的「使用说明」页 + README.md 为准，避免两份文案各自过期。

Write-Output '[4/5] 算 SHA256 ...'
$sums = Join-Path $dist 'SHA256SUMS.txt'
$lines = @()
foreach ($f in @($setup)) {
  $h = (Get-FileHash $f -Algorithm SHA256).Hash.ToLower()
  $lines += "$h  $(Split-Path $f -Leaf)"
}
Set-Content -Path $sums -Value $lines -Encoding ASCII

Write-Output '[5/5] 汇总 ...'
$mbSetup = (Get-Item $setup).Length / 1MB
Write-Output ('  安装包 {0:N2} MB' -f $mbSetup)
Write-Output ''
Write-Output '=== dist 产物 ==='
Get-ChildItem $dist | Where-Object { $_.Name -like "*$version*" -or $_.Name -eq 'SHA256SUMS.txt' } |
  Select-Object Name, @{n='大小';e={ if ($_.PSIsContainer) { '<目录>' } else { '{0:N2} MB' -f ($_.Length/1MB) } }} |
  Format-Table -AutoSize | Out-String
Write-Output '发布前记得：把 dist 里的 setup.exe 与 SHA256SUMS.txt 一起上传到 release 资产。'
