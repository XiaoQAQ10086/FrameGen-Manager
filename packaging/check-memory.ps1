# Measure the memory the shipped build actually uses, and fail if it is over budget.
#
# The project has hard budgets: the installer stays under 10 MB, memory under
# 60 MB, and closing the window exits the process. This covers the memory one.
#
# The number read here is "Working Set - Private", which is the figure Task
# Manager shows in its Memory column. The script starts the release build,
# waits for it to settle, samples a few times, then closes it.
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File packaging\check-memory.ps1
#   powershell -ExecutionPolicy Bypass -File packaging\check-memory.ps1 -BudgetMB 60

param(
    [double]$BudgetMB = 60,
    [int]$SettleSeconds = 8,
    [int]$Samples = 3
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $root 'target\release\framegen-manager.exe'
if (-not (Test-Path $exe)) {
    throw "release build not found: $exe - run 'cargo build --release' first"
}

Write-Output "measuring $exe"
$proc = Start-Process -FilePath $exe -PassThru
$peak = 0.0
try {
    Start-Sleep -Seconds $SettleSeconds
    for ($i = 1; $i -le $Samples; $i++) {
        Start-Sleep -Seconds 2
        $p = Get-Process -Id $proc.Id -ErrorAction SilentlyContinue
        if (-not $p) { throw 'the app exited on its own' }
        $p.Refresh()
        $c = Get-Counter '\Process(framegen-manager)\Working Set - Private' -ErrorAction SilentlyContinue
        $privateMB = if ($c) { $c.CounterSamples[0].CookedValue / 1MB } else { $p.PrivateMemorySize64 / 1MB }
        $totalMB = $p.WorkingSet64 / 1MB
        Write-Output ("  sample {0}: private {1:N1} MB   total working set {2:N1} MB" -f $i, $privateMB, $totalMB)
        if ($privateMB -gt $peak) { $peak = $privateMB }
    }
} finally {
    if (Get-Process -Id $proc.Id -ErrorAction SilentlyContinue) { Stop-Process -Id $proc.Id -Force }
}

Write-Output ("private working set peak = {0:N1} MB (budget {1:N1} MB)" -f $peak, $BudgetMB)
if ($peak -ge $BudgetMB) {
    Write-Output 'OVER BUDGET'
    exit 1
}
Write-Output 'within budget'
