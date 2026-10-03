# FrameGen Manager —— 发布打包脚本
#
# 用法：  powershell -ExecutionPolicy Bypass -File packaging\build-release.ps1
#
# 产出：
#   dist\FrameGen-Manager-v<版本>\        便携版目录
#   dist\FrameGen-Manager-v<版本>.zip     便携版压缩包（发给用户的就是它）
#   dist\SHA256SUMS.txt                   校验和
#   dist\<...>-setup.exe                  Inno Setup 安装包（装了 Inno 才会出）

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

# 版本号只从 Cargo.toml 读，避免两处不一致
$m = Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
if (-not $m) { throw 'Cargo.toml 里找不到 version' }
$version = $m.Matches[0].Groups[1].Value
$stageName = "FrameGen-Manager-v$version"
Write-Output "版本 = $version"

Write-Output '[1/5] 编译 release ...'
cargo build --release
$exe = Join-Path $root 'target\release\framegen-manager.exe'
if (-not (Test-Path $exe)) { throw "没找到 $exe" }

# 打包前先让 exe 自己体检一遍：自测必须全过（退出码 0），不过就不出包。
# 这一步专门拦「静默变坏」—— 最典型的是 build.rs 找不到 packaging\app.ico 时
# 只发警告不报错、exe 就没图标了；自测里那两条会红，退出码变成 1。
Write-Output '[1.5/5] 跑 release 自测（不过就不许打包）...'
$testOut = & $exe --selftest 2>&1 | Out-String
if ($LASTEXITCODE -ne 0) {
  Write-Output $testOut
  throw "release 自测没过（退出码 $LASTEXITCODE），已中止打包"
}
Write-Output ('  自测通过（' + ([regex]::Matches($testOut, '\[PASS\]')).Count + ' 项全过）')

Write-Output '[2/5] 组装便携版目录 ...'
$dist = Join-Path $root 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$stage = Join-Path $dist $stageName
if (Test-Path $stage) { Remove-Item $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null

Copy-Item $exe $stage
Copy-Item (Join-Path $root 'packaging\使用说明.txt') (Join-Path $stage '使用说明.txt')
foreach ($extra in @('README.md', 'LICENSE', 'LICENSE.txt', 'THIRD_PARTY_NOTICES.txt')) {
  $p = Join-Path $root $extra
  if (Test-Path $p) { Copy-Item $p $stage }
}

Write-Output '[3/5] 打 zip ...'
$zip = Join-Path $dist "$stageName.zip"
if (Test-Path $zip) { Remove-Item $zip -Force }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal

Write-Output '[4/5] 算 SHA256 ...'
$sums = Join-Path $dist 'SHA256SUMS.txt'
$lines = @()
$h = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
$lines += "$h  $(Split-Path $zip -Leaf)"
Set-Content -Path $sums -Value $lines -Encoding ASCII

Write-Output '[5/5] 尝试出安装包 ...'
$iscc = $null
foreach ($p in @(
  'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
  'C:\Program Files\Inno Setup 6\ISCC.exe'
)) { if (Test-Path $p) { $iscc = $p; break } }

if ($iscc) {
  & $iscc "/DAppVersion=$version" (Join-Path $root 'packaging\installer.iss')
  $setup = Join-Path $dist "$stageName-setup.exe"
  if (Test-Path $setup) {
    $h2 = (Get-FileHash $setup -Algorithm SHA256).Hash.ToLower()
    Add-Content -Path $sums -Value "$h2  $(Split-Path $setup -Leaf)"
    # 硬指标：安装包必须 < 10 MB。超了就别发出去。
    $mb = (Get-Item $setup).Length / 1MB
    if ($mb -ge 10) { throw ("安装包 {0:N2} MB，超过 10 MB 的硬指标，已中止" -f $mb) }
    Write-Output ('  安装包 {0:N2} MB（< 10 MB 达标）' -f $mb)
  }
} else {
  Write-Output '  没装 Inno Setup，跳过安装包。想要的话装一下 Inno Setup 6 再跑一次。'
}

Write-Output ''
Write-Output '=== dist 产物 ==='
Get-ChildItem $dist | Select-Object Name,
  @{n='大小';e={ if ($_.PSIsContainer) { '<目录>' } else { '{0:N2} MB' -f ($_.Length/1MB) } }} |
  Format-Table -AutoSize | Out-String
