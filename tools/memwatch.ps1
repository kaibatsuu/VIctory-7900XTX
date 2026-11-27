# Samples a running victory.exe from outside: RAM (working set, private bytes)
# and VRAM (the card's memory and the PC's memory shared with the card, as the
# GPU's counters charge them to the process) to a CSV, one line a sample.
#
#   tools\memwatch.ps1 -ProcessName victory -Minutes 10
#   tools\memwatch.ps1 -ProcessName victory -Once
#
# It needs no rebuild: it watches the release build too, and says nothing the
# layer does not already show Windows. For a build with the diagnostics
# feature, `VICTORY_MEMLOG=<file>` (docs/memory.md) writes the layer's own
# breakdown, which this cannot see; the two files can be read side by side.
# A line a sample while it runs, a summary at the end, and the CSV for
# `tools\memlog.ps1`-style trending in a spreadsheet.
#
# The GPU counters ("GPU Process Memory", "GPU Adapter Memory") exist on
# Windows 10 1709 and later with a WDDM 2.0 driver. Where they are missing
# (or -NoGpu is given) the VRAM columns are 0 and the RAM columns still work.
param(
    [string]$ProcessName = 'victory',
    [double]$IntervalSeconds = 2,
    [double]$Minutes = 0,
    [string]$Out,
    [switch]$Once,
    [switch]$NoGpu
)

$ErrorActionPreference = 'Stop'
if ($IntervalSeconds -lt 0.5) { $IntervalSeconds = 0.5 }
$script:BaseName = $ProcessName -replace '\.exe$', ''

function Find-Target {
    # The biggest process of that name: the title, not a launcher.
    Get-Process -Name $script:BaseName -ErrorAction SilentlyContinue |
        Sort-Object WorkingSet64 -Descending | Select-Object -First 1
}

function Get-GpuByName([string]$Name, [string]$Counter) {
    if ($NoGpu) { return [int64]0 }
    try {
        $samples = (Get-Counter -Counter "\GPU Process Memory(*)\$Counter" -ErrorAction Stop).CounterSamples
    } catch {
        return [int64]0
    }
    $sum = [int64]0
    foreach ($sample in $samples) {
        if ($sample.CookedValue -le 0) { continue }
        if ($sample.InstanceName -notmatch 'pid_(\d+)_') { continue }
        $proc = Get-Process -Id ([int]$Matches[1]) -ErrorAction SilentlyContinue
        if ($proc -and $proc.ProcessName -eq $Name) { $sum += [int64]$sample.CookedValue }
    }
    $sum
}

function Get-AdapterDedicated {
    if ($NoGpu) { return [int64]0 }
    try {
        $samples = (Get-Counter -Counter '\GPU Adapter Memory(*)\Dedicated Usage' -ErrorAction Stop).CounterSamples
    } catch {
        return [int64]0
    }
    $sum = [int64]0
    foreach ($sample in $samples) {
        if ($sample.CookedValue -gt 0) { $sum += [int64]$sample.CookedValue }
    }
    $sum
}

function New-Sample($Process) {
    $mb = 1MB
    $null = $Process.Refresh()
    [pscustomobject]@{
        time             = (Get-Date).ToString('yyyy-MM-dd HH:mm:ss')
        elapsed_s        = $script:elapsed
        pid              = $Process.Id
        working_set_mb   = [math]::Round($Process.WorkingSet64 / $mb, 1)
        private_mb       = [math]::Round($Process.PrivateMemorySize64 / $mb, 1)
        commit_mb        = [math]::Round($Process.PagedMemorySize64 / $mb, 1)
        vram_local_mb    = [math]::Round((Get-GpuByName $script:BaseName 'Local Usage') / $mb, 1)
        vram_shared_mb   = [math]::Round((Get-GpuByName $script:BaseName 'Non Local Usage') / $mb, 1)
        vram_committed_mb = [math]::Round((Get-GpuByName $script:BaseName 'Total Committed') / $mb, 1)
        adapter_dedicated_mb = [math]::Round((Get-AdapterDedicated) / $mb, 1)
    }
}

function Write-Summary($Samples) {
    if ($Samples.Count -lt 2) { return }
    $first = $Samples[0]
    $last = $Samples[-1]
    $minutes = [math]::Max(($last.elapsed_s - $first.elapsed_s) / 60.0, 0.001)
    Write-Host ''
    Write-Host ("watched {0:N1} min, {1} samples, every {2:N1} s" -f $minutes, $Samples.Count, $IntervalSeconds) -ForegroundColor Cyan
    foreach ($field in 'working_set_mb', 'private_mb', 'vram_local_mb', 'vram_shared_mb') {
        $start = $first.$field
        $end = $last.$field
        $peak = ($Samples | Measure-Object $field -Maximum).Maximum
        $rate = ($end - $start) / $minutes
        Write-Host ("  {0,-16} {1,9:N0} -> {2,9:N0} MB  peak {3,9:N0}  {4,8:N1} MB/min" -f $field, $start, $end, $peak, $rate)
    }
    # The steepest rise between any two samples within a minute: a load, or a leak starting.
    $window = $Samples | Where-Object { $_.elapsed_s -ge ($last.elapsed_s - 60) }
    if ($window.Count -ge 2) {
        Write-Host ("  last 60 s: private {0:N0} -> {1:N0} MB, VRAM {2:N0} -> {3:N0} MB" -f `
            $window[0].private_mb, $last.private_mb, $window[0].vram_local_mb, $last.vram_local_mb)
    }
}

$script:elapsed = 0.0
$target = Find-Target
if (-not $target) { Write-Host "No process named '$ProcessName' is running." -ForegroundColor Yellow; exit 1 }
if (-not $Out) {
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $Out = Join-Path $env:TEMP "victory-memwatch-$stamp.csv"
}
$folder = Split-Path -Parent $Out
if ($folder -and -not (Test-Path $folder)) { $null = New-Item -ItemType Directory -Path $folder -Force }

Write-Host ("watching {0} (pid {1}) every {2:N1} s -> {3}" -f $target.ProcessName, $target.Id, $IntervalSeconds, $Out) -ForegroundColor Cyan
$samples = New-Object System.Collections.Generic.List[object]
$clock = [System.Diagnostics.Stopwatch]::StartNew()
$written = $false
$limit = if ($Minutes -gt 0) { $Minutes * 60 } else { [double]::MaxValue }

while ($true) {
    $target = Find-Target
    if (-not $target) {
        Write-Host 'the process is gone' -ForegroundColor Yellow
        break
    }
    $script:elapsed = [math]::Round($clock.Elapsed.TotalSeconds, 1)
    $sample = New-Sample $target
    $samples.Add($sample)
    $line = ($sample.PSObject.Properties | ForEach-Object { $_.Value }) -join ','
    if (-not $written) {
        $header = ($sample.PSObject.Properties | ForEach-Object { $_.Name }) -join ','
        Set-Content -Path $Out -Value $header -Encoding UTF8
        $written = $true
    }
    Add-Content -Path $Out -Value $line -Encoding UTF8
    if ($samples.Count -eq 1 -or $samples.Count % 5 -eq 0) {
        Write-Host ("{0,8:N0} s  private {1,7:N0} MB  working {2,7:N0} MB  VRAM {3,7:N0} MB  shared {4,7:N0} MB  adapter {5,7:N0} MB" -f `
            $script:elapsed, $sample.private_mb, $sample.working_set_mb, $sample.vram_local_mb, $sample.vram_shared_mb, $sample.adapter_dedicated_mb)
    }
    if ($Once) { break }
    if ($clock.Elapsed.TotalSeconds -ge $limit) { break }
    Start-Sleep -Milliseconds ([int]($IntervalSeconds * 1000))
}

if (-not $Once) { Write-Summary $samples }
Write-Host "the samples are in $Out" -ForegroundColor Cyan
