# PaceDock portable update helper - isolated behavioral tests
# Usage: powershell -ExecutionPolicy Bypass -File scripts\test-update-helper.ps1
# Scenarios: A (success), B (post-backup failure), C (log truncation)

param(
    [switch]$SkipCleanup
)

$ErrorActionPreference = "Stop"
# Ensure core utility module is loaded (some constrained environments skip autoload)
Import-Module Microsoft.PowerShell.Utility -ErrorAction SilentlyContinue
$TestRoot = Join-Path $env:TEMP "fa_helper_test_$(Get-Date -Format 'yyyyMMdd_HHmmss')"
$Results = @()
$utf8Bom = [byte[]]@(0xEF, 0xBB, 0xBF)
$markerName = ".pacedock-portable"

function Assert-Condition {
    param([string]$Test, [bool]$Condition, [string]$Detail)
    if ($Condition) {
        Write-Host "  PASS: $Test" -ForegroundColor Green
        $script:Results += @{ Test = $Test; Pass = $true; Detail = $Detail }
    } else {
        Write-Host "  FAIL: $Test - $Detail" -ForegroundColor Red
        $script:Results += @{ Test = $Test; Pass = $false; Detail = $Detail }
    }
}

function New-FakeExe {
    param([string]$Path, [string]$Content, [string]$SourceExe)
    $dir = Split-Path $Path -Parent
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    # Use where.exe (exits immediately, non-interactive) as default stub
    $src = if ($SourceExe) { $SourceExe } else { "$env:SystemRoot\System32\where.exe" }
    Copy-Item $src $Path -Force
    Write-Host "  created: $Path"
}

function New-TextFile {
    param([string]$Path, [string]$Content)
    $dir = Split-Path $Path -Parent
    if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }
    [System.IO.File]::WriteAllText($Path, $Content, [System.Text.Encoding]::UTF8)
    Write-Host "  created: $Path"
}

function Write-ScriptBom {
    param([string]$Path, [string]$Content)
    $scriptBytes = $utf8Bom + [System.Text.Encoding]::UTF8.GetBytes($Content)
    [System.IO.File]::WriteAllBytes($Path, $scriptBytes)
}

# Build the helper script template string. This template MIRRORS
# src-tauri/src/update.rs::portable_helper_script and must be kept in sync:
# resource swap/rollback, parenthesized elseif conditions, restart order.
# Paths are substituted via string interpolation in the double-quoted
# here-string; script-internal $vars are escaped with backtick so they pass
# through as literal dollar signs. -FailAfterSwap is a TEST-ONLY hook to force
# a failure after the resource swap; the production script has no such line.
function Build-Script {
    param(
        [string]$Old, [string]$New, [string]$Marker, [string]$Log,
        [int]$TargetPid, [string]$NewResources, [string]$FailAfterSwap
    )
    return @"
# PaceDock portable update helper (mirrors src-tauri/src/update.rs)
param(
    [int]`$TargetPid = $TargetPid,
    [string]`$FailAfterSwap
)

`$ErrorActionPreference = "Stop"
`$LogFile = '$Log'

# Truncate log on each invocation
Remove-Item `$LogFile -Force -ErrorAction SilentlyContinue

function Write-Log {
    param([string]`$Message)
    `$ts = Get-Date -Format "yyyy-MM-dd HH:mm:ss"
    "`$ts `$Message" | Out-File -FilePath `$LogFile -Append -Encoding utf8
}

Write-Log "helper started, target PID=`$TargetPid"

`$OldExe = '$Old'
`$NewExe = '$New'
`$Marker = '$Marker'
`$OldDir = Split-Path `$OldExe -Parent

Write-Log "old=`$OldExe, new=`$NewExe, marker=`$Marker"

# Wait for PaceDock to exit (with timeout)
`$timeout = Get-Date
while (`$true) {
    `$proc = Get-Process -Id `$TargetPid -ErrorAction SilentlyContinue
    if (-not `$proc) { break }
    if (((Get-Date) - `$timeout).TotalSeconds -gt 30) {
        Write-Log "ERROR: timeout waiting for PID `$TargetPid"
        exit 1
    }
    Start-Sleep -Milliseconds 200
}

Write-Log "target exited, waiting for file unlock"
Start-Sleep -Milliseconds 500

# Backup, replace, marker, resources, cleanup all inside try/catch
`$ResourcesDir = Join-Path `$OldDir "resources\benchmark"
`$ResourcesBackup = Join-Path `$OldDir "resources\benchmark.bak"
`$OldResourcesExisted = Test-Path `$ResourcesDir
`$Backup = "`$OldExe.bak"
try {
    # Backup old exe
    Write-Log "creating backup: `$Backup"
    Copy-Item -Path `$OldExe -Destination `$Backup -Force -ErrorAction Stop
    Write-Log "backup OK"

    # Replace with new exe (move is closer to atomic than copy+delete)
    Write-Log "replacing exe"
    Move-Item -Path `$NewExe -Destination `$OldExe -Force -ErrorAction Stop
    Write-Log "replace OK"

    # Copy marker
    if (Test-Path `$Marker) {
        Write-Log "copying marker"
        Copy-Item -Path `$Marker -Destination (Join-Path `$OldDir "$markerName") -Force
        Remove-Item -Path `$Marker -Force -ErrorAction SilentlyContinue
        Write-Log "marker OK"
    }

    # Swap benchmark resources: old resources renamed to backup, then whole new dir moved in
    Write-Log "swapping benchmark resources"
    if (`$OldResourcesExisted) {
        Rename-Item -Path `$ResourcesDir -NewName "benchmark.bak" -Force -ErrorAction Stop
        Write-Log "old resources backed up to benchmark.bak"
    } else {
        New-Item -ItemType Directory -Force -Path (Split-Path `$ResourcesDir -Parent) | Out-Null
    }
    Move-Item -Path '$NewResources' -Destination `$ResourcesDir -Force -ErrorAction Stop
    Write-Log "resources swapped"

    # TEST HOOK ONLY: force failure after resource swap (production has no such line).
    # Guard with `-and` because Test-Path $null throws an empty-string binding error.
    if (`$FailAfterSwap -and (Test-Path `$FailAfterSwap)) {
        Write-Log "ERROR: test-induced failure after resource swap"
        throw "test-induced failure after resource swap"
    }

    # Cleanup backups only on success
    Write-Log "removing backups"
    Remove-Item -Path `$Backup -Force -ErrorAction SilentlyContinue
    if (Test-Path `$ResourcesBackup) {
        Remove-Item -Path `$ResourcesBackup -Recurse -Force -ErrorAction SilentlyContinue
    }
    Write-Log "backups cleaned"
} catch {
    Write-Log "ERROR: `$(`$_.Exception.Message)"
    # Backup exists = mutation occurred, restore; no backup = old exe untouched
    `$exeRestored = `$false
    if (Test-Path `$Backup) {
        Write-Log "restoring exe from backup"
        Move-Item -Path `$Backup -Destination `$OldExe -Force -ErrorAction SilentlyContinue
        Write-Log "exe restored"
        `$exeRestored = `$true
    } else {
        Write-Log "ERROR: failure before backup, old exe untouched"
    }
    # Restore resources: old existed -> swap backup back; old absent -> remove partial new
    if (Test-Path `$ResourcesBackup) {
        Write-Log "restoring resources from backup"
        Remove-Item -Path `$ResourcesDir -Recurse -Force -ErrorAction SilentlyContinue
        Rename-Item -Path `$ResourcesBackup -NewName "benchmark" -Force -ErrorAction SilentlyContinue
        Write-Log "resources restored"
    } elseif ((Test-Path `$ResourcesDir) -and -not `$OldResourcesExisted) {
        Write-Log "removing partially installed resources"
        Remove-Item -Path `$ResourcesDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Log "partial resources removed"
    }
    # Restart original only after rollback completes
    if (`$exeRestored) {
        Write-Log "restarting original"
        Start-Process -FilePath `$OldExe
        Write-Log "original restart initiated"
    }
    exit 1
}

# Success: restart
Write-Log "SUCCESS, restarting `$OldExe"
Start-Process -FilePath `$OldExe
Write-Log "restart initiated"
"@
}

# ==================================================================
# Scenario A: Success
# ==================================================================

Write-Host "`n=== Scenario A: Success ===" -ForegroundColor Cyan

$dirA = Join-Path $TestRoot "A_success"
$tmpA = Join-Path $dirA "temp"
New-Item -ItemType Directory -Path $dirA -Force | Out-Null
New-Item -ItemType Directory -Path $tmpA -Force | Out-Null

# Production layout: old exe at install dir, new exe + marker in temp
$oldA = Join-Path $dirA "PaceDock.exe"
$newA = Join-Path $tmpA "PaceDock_new.exe"
$markerA = Join-Path $tmpA $markerName
$logA = Join-Path $tmpA "update.log"

# Use non-interactive executables as stubs so Start-Process doesn't block
$srcOldExe = "$env:SystemRoot\System32\where.exe"
$srcNewExe = "$env:SystemRoot\System32\whoami.exe"

New-FakeExe $oldA "OLD" -SourceExe $srcOldExe
New-FakeExe $newA "NEW" -SourceExe $srcNewExe
New-TextFile $markerA "marker-data"
$newResA = Join-Path $tmpA "benchmark"
New-TextFile (Join-Path $newResA "asset.txt") "NEW_ASSET"
$hashOldA = (Get-FileHash $oldA -Algorithm SHA256).Hash
$hashNewA = (Get-FileHash $newA -Algorithm SHA256).Hash

$scriptA = Build-Script -Old $oldA -New $newA -Marker $markerA -Log $logA -TargetPid 999999 -NewResources $newResA
$scriptPathA = Join-Path $dirA "update.ps1"
Write-ScriptBom $scriptPathA $scriptA

Write-Host "  Running helper script..."
$procA = Start-Process -FilePath "powershell" -ArgumentList "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File", $scriptPathA -Wait -NoNewWindow -PassThru

$backupA = "$oldA.bak"
Assert-Condition "A1: old exe replaced (hash match)" `
    ((Get-FileHash $oldA -Algorithm SHA256).Hash -eq $hashNewA) `
    "Old hash: $((Get-FileHash $oldA -Algorithm SHA256).Hash), expected: $hashNewA"

Assert-Condition "A2: backup removed" (-not (Test-Path $backupA))

Assert-Condition "A3: marker copied" `
    ((Test-Path (Join-Path $dirA $markerName)) -and (Get-Content (Join-Path $dirA $markerName) -Raw).Trim() -eq "marker-data")

Assert-Condition "A4: temp marker removed" (-not (Test-Path $markerA))

Assert-Condition "A5: log contains SUCCESS" `
    ((Test-Path $logA) -and ((Get-Content $logA -Raw) -match "SUCCESS"))

Assert-Condition "A6: log contains restart initiated" `
    ((Test-Path $logA) -and ((Get-Content $logA -Raw) -match "restart initiated"))

Assert-Condition "A7: exit code 0" ($procA.ExitCode -eq 0) "Got: $($procA.ExitCode)"

Assert-Condition "A8: resources installed" `
    ((Test-Path (Join-Path $dirA "resources\benchmark\asset.txt")) -and (Get-Content (Join-Path $dirA "resources\benchmark\asset.txt") -Raw).Trim() -eq "NEW_ASSET")

# ==================================================================
# Scenario B: Post-backup failure (Move-Item fails after backup)
# ==================================================================

Write-Host "`n=== Scenario B: Post-backup failure ===" -ForegroundColor Cyan

$dirB = Join-Path $TestRoot "B_failure"
$tmpB = Join-Path $dirB "temp"
New-Item -ItemType Directory -Path $dirB -Force | Out-Null
New-Item -ItemType Directory -Path $tmpB -Force | Out-Null

$oldB = Join-Path $dirB "PaceDock.exe"
$newB = Join-Path $tmpB "PaceDock_new.exe"
$markerB = Join-Path $tmpB $markerName
$logB = Join-Path $tmpB "update.log"

# Backup copies old exe (file, real cmd.exe) successfully.
# But new exe path is a directory, so Move-Item file->dir fails.
New-FakeExe $oldB "ORIGINAL" -SourceExe $srcOldExe
$hashOriginalB = (Get-FileHash $oldB -Algorithm SHA256).Hash
New-Item -ItemType Directory -Path $newB -Force | Out-Null

$newResB = Join-Path $tmpB "benchmark"
$scriptB = Build-Script -Old $oldB -New $newB -Marker $markerB -Log $logB -TargetPid 999999 -NewResources $newResB
$scriptPathB = Join-Path $dirB "update.ps1"
Write-ScriptBom $scriptPathB $scriptB

Write-Host "  Running helper script (expecting Move-Item failure)..."
$procB = Start-Process -FilePath "powershell" -ArgumentList "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File", $scriptPathB -Wait -NoNewWindow -PassThru

$backupB = "$oldB.bak"
$oldExists = Test-Path $oldB
$oldIsFile = if ($oldExists) { (Get-Item $oldB) -is [System.IO.FileInfo] } else { $false }

Assert-Condition "B1: old exe restored as file" `
    ($oldIsFile) "Old: $(if ($oldExists) { (Get-Item $oldB).GetType().Name } else { 'missing' })"

Assert-Condition "B2: content preserved (hash match)" `
    ((Get-FileHash $oldB -Algorithm SHA256).Hash -eq $hashOriginalB) `
    "Old hash: $((Get-FileHash $oldB -Algorithm SHA256).Hash), expected: $hashOriginalB"

Assert-Condition "B3: backup cleaned after restore" (-not (Test-Path $backupB))

Assert-Condition "B4: log has ERROR" `
    ((Test-Path $logB) -and ((Get-Content $logB -Raw) -match "ERROR"))

Assert-Condition "B5: log has restoring exe from backup" `
    ((Test-Path $logB) -and ((Get-Content $logB -Raw) -match "restoring exe from backup"))

Assert-Condition "B6: exit code 1" ($procB.ExitCode -eq 1) "Got: $($procB.ExitCode)"

Assert-Condition "B7: log has original restart initiated" `
    ((Test-Path $logB) -and ((Get-Content $logB -Raw) -match "original restart initiated"))

# ==================================================================
# Scenario C: Log truncation
# ==================================================================

Write-Host "`n=== Scenario C: Log truncation ===" -ForegroundColor Cyan

$dirC = Join-Path $TestRoot "C_log_truncation"
$tmpC = Join-Path $dirC "temp"
New-Item -ItemType Directory -Path $dirC -Force | Out-Null
New-Item -ItemType Directory -Path $tmpC -Force | Out-Null

$oldC = Join-Path $dirC "PaceDock.exe"
$newC = Join-Path $tmpC "PaceDock_new.exe"
$markerC = Join-Path $tmpC $markerName
$logC = Join-Path $tmpC "update.log"

# Pre-populate with stale content (simulate multiple prior runs)
$staleContent = (1..500 | ForEach-Object { "STALE_LOG_LINE_$_" }) -join "`r`n"
[System.IO.File]::WriteAllText($logC, $staleContent, [System.Text.Encoding]::UTF8)
$staleSize = (Get-Item $logC).Length

New-FakeExe $oldC "OLD" -SourceExe $srcOldExe
New-FakeExe $newC "NEW" -SourceExe $srcNewExe
New-TextFile $markerC "marker"
$newResC = Join-Path $tmpC "benchmark"
New-TextFile (Join-Path $newResC "asset.txt") "NEW_ASSET"

$scriptC = Build-Script -Old $oldC -New $newC -Marker $markerC -Log $logC -TargetPid 999999 -NewResources $newResC
$scriptPathC = Join-Path $dirC "update.ps1"
Write-ScriptBom $scriptPathC $scriptC

Write-Host "  Running helper script..."
$procC = Start-Process -FilePath "powershell" -ArgumentList "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File", $scriptPathC -Wait -NoNewWindow -PassThru

$newSize = (Get-Item $logC).Length
$logContent = Get-Content $logC -Raw

Assert-Condition "C1: log truncated ($newSize < $staleSize)" ($newSize -lt $staleSize)
Assert-Condition "C2: no stale lines" ($logContent -notmatch "STALE_LOG_LINE")
Assert-Condition "C3: has current markers" ($logContent -match "helper started" -and $logContent -match "SUCCESS")
Assert-Condition "C4: exit code 0" ($procC.ExitCode -eq 0) "Got: $($procC.ExitCode)"

# ==================================================================
# Scenario D: Old resources existed, failure after swap -> restore from backup
# ==================================================================

Write-Host "`n=== Scenario D: old resources existed, fail after swap ===" -ForegroundColor Cyan

$dirD = Join-Path $TestRoot "D_rollback_restore"
$tmpD = Join-Path $dirD "temp"
New-Item -ItemType Directory -Path $dirD -Force | Out-Null
New-Item -ItemType Directory -Path $tmpD -Force | Out-Null

$oldD = Join-Path $dirD "PaceDock.exe"
$newD = Join-Path $tmpD "PaceDock_new.exe"
$markerD = Join-Path $tmpD $markerName
$logD = Join-Path $tmpD "update.log"
$failD = Join-Path $tmpD "fail_after_swap.marker"

New-FakeExe $oldD "ORIGINAL" -SourceExe $srcOldExe
New-FakeExe $newD "NEW" -SourceExe $srcNewExe
New-TextFile $markerD "marker-data"
# Old resources exist (production layout)
$oldResD = Join-Path $dirD "resources\benchmark"
New-TextFile (Join-Path $oldResD "old_asset.txt") "OLD_ASSET"
# New resources staged in temp; failure marker forces error right after swap
$newResD = Join-Path $tmpD "benchmark"
New-TextFile (Join-Path $newResD "new_asset.txt") "NEW_ASSET"
New-TextFile $failD "boom"
$hashOldD = (Get-FileHash $oldD -Algorithm SHA256).Hash

$scriptD = Build-Script -Old $oldD -New $newD -Marker $markerD -Log $logD -TargetPid 999999 -NewResources $newResD -FailAfterSwap $failD
$scriptPathD = Join-Path $dirD "update.ps1"
Write-ScriptBom $scriptPathD $scriptD

Write-Host "  Running helper script (expecting failure after swap)..."
$procD = Start-Process -FilePath "powershell" -ArgumentList "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File", $scriptPathD, "-TargetPid", "999999", "-FailAfterSwap", $failD -Wait -NoNewWindow -PassThru

Assert-Condition "D1: old exe restored (hash match)" `
    ((Get-FileHash $oldD -Algorithm SHA256).Hash -eq $hashOldD) `
    "Old hash: $((Get-FileHash $oldD -Algorithm SHA256).Hash), expected: $hashOldD"

Assert-Condition "D2: old resources restored" `
    ((Test-Path (Join-Path $oldResD "old_asset.txt")) -and (Get-Content (Join-Path $oldResD "old_asset.txt") -Raw).Trim() -eq "OLD_ASSET")

Assert-Condition "D3: benchmark.bak removed after restore" `
    (-not (Test-Path (Join-Path $dirD "resources\benchmark.bak")))

Assert-Condition "D4: new resources removed after restore" `
    (-not (Test-Path (Join-Path $oldResD "new_asset.txt")))

Assert-Condition "D5: log has restoring resources from backup" `
    ((Test-Path $logD) -and ((Get-Content $logD -Raw) -match "restoring resources from backup"))

Assert-Condition "D6: exit code 1" ($procD.ExitCode -eq 1) "Got: $($procD.ExitCode)"

Assert-Condition "D7: log has original restart initiated" `
    ((Test-Path $logD) -and ((Get-Content $logD -Raw) -match "original restart initiated"))

# ==================================================================
# Scenario E: Old resources absent, failure after swap -> remove partial new resources
# ==================================================================

Write-Host "`n=== Scenario E: old resources absent, fail after swap ===" -ForegroundColor Cyan

$dirE = Join-Path $TestRoot "E_rollback_partial_removal"
$tmpE = Join-Path $dirE "temp"
New-Item -ItemType Directory -Path $dirE -Force | Out-Null
New-Item -ItemType Directory -Path $tmpE -Force | Out-Null

$oldE = Join-Path $dirE "PaceDock.exe"
$newE = Join-Path $tmpE "PaceDock_new.exe"
$markerE = Join-Path $tmpE $markerName
$logE = Join-Path $tmpE "update.log"
$failE = Join-Path $tmpE "fail_after_swap.marker"

New-FakeExe $oldE "ORIGINAL" -SourceExe $srcOldExe
New-FakeExe $newE "NEW" -SourceExe $srcNewExe
New-TextFile $markerE "marker-data"
# NO old resources directory (fresh install)
$newResE = Join-Path $tmpE "benchmark"
New-TextFile (Join-Path $newResE "new_asset.txt") "NEW_ASSET"
New-TextFile $failE "boom"
$hashOldE = (Get-FileHash $oldE -Algorithm SHA256).Hash

$scriptE = Build-Script -Old $oldE -New $newE -Marker $markerE -Log $logE -TargetPid 999999 -NewResources $newResE -FailAfterSwap $failE
$scriptPathE = Join-Path $dirE "update.ps1"
Write-ScriptBom $scriptPathE $scriptE

Write-Host "  Running helper script (expecting failure after swap)..."
$procE = Start-Process -FilePath "powershell" -ArgumentList "-WindowStyle", "Hidden", "-ExecutionPolicy", "Bypass", "-File", $scriptPathE, "-TargetPid", "999999", "-FailAfterSwap", $failE -Wait -NoNewWindow -PassThru

$resDirE = Join-Path $dirE "resources\benchmark"

Assert-Condition "E1: old exe restored (hash match)" `
    ((Get-FileHash $oldE -Algorithm SHA256).Hash -eq $hashOldE) `
    "Old hash: $((Get-FileHash $oldE -Algorithm SHA256).Hash), expected: $hashOldE"

Assert-Condition "E2: partial new resources removed (benchmark absent)" `
    (-not (Test-Path $resDirE))

Assert-Condition "E3: log has removing partially installed resources" `
    ((Test-Path $logE) -and ((Get-Content $logE -Raw) -match "removing partially installed resources"))

Assert-Condition "E4: log has partial resources removed" `
    ((Test-Path $logE) -and ((Get-Content $logE -Raw) -match "partial resources removed"))

Assert-Condition "E5: exit code 1" ($procE.ExitCode -eq 1) "Got: $($procE.ExitCode)"

Assert-Condition "E6: log has original restart initiated" `
    ((Test-Path $logE) -and ((Get-Content $logE -Raw) -match "original restart initiated"))

# ==================================================================
# Summary
# ==================================================================

$passCount = ($Results | Where-Object { $_.Pass }).Count
$failCount = ($Results | Where-Object { -not $_.Pass }).Count
$totalCount = $Results.Count

Write-Host "`n========================================" -ForegroundColor White
Write-Host "  Behavioral Test Results" -ForegroundColor White
Write-Host "========================================" -ForegroundColor White
Write-Host "  Total: $totalCount  Pass: $passCount  Fail: $failCount" -ForegroundColor $(if ($failCount -eq 0) { "Green" } else { "Red" })

if ($failCount -gt 0) {
    Write-Host "`n  Failures:" -ForegroundColor Red
    $Results | Where-Object { -not $_.Pass } | ForEach-Object {
        Write-Host "    $($_.Test): $($_.Detail)" -ForegroundColor Red
    }
}

if (-not $SkipCleanup) {
    Remove-Item -Path $TestRoot -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "`n  Cleaned: $TestRoot"
} else {
    Write-Host "`n  Kept: $TestRoot"
}

if ($failCount -gt 0) { exit 1 }
Write-Host "`n  All behavioral tests passed.`n"
exit 0
