# Reads a memory log and says what grew: one line a sample, one column a
# number, as `VICTORY_MEMLOG=<file>` writes it (docs/memory.md) or as
# `tools\memwatch.ps1` writes its outside view of the process.
#
#   tools\memlog.ps1 -Csv $env:TEMP\victory-memlog.csv
#   tools\memlog.ps1 -Csv $env:TEMP\victory-memlog.csv -WindowSeconds 30 -Top 20
#
# For every column: the first and last value, the change, the change a minute,
# and the steepest rise inside one window (a load, or a leak starting). The
# columns that grew most come first, then the ones that shrank, then a
# timeline of the run. Columns ending in `_mb` are megabytes; the others are
# counts or bytes as named. Categories overlap (the memory line says so too),
# so do not add them up; read the columns one by one.
param(
    [Parameter(Mandatory)][string]$Csv,
    [double]$WindowSeconds = 60,
    [int]$Top = 12
)

$ErrorActionPreference = 'Stop'
if (-not (Test-Path $Csv)) { Write-Host "No $Csv." -ForegroundColor Yellow; exit 1 }
$rows = @(Import-Csv -Path $Csv)
if ($rows.Count -lt 2) { Write-Host "Only $($rows.Count) sample(s) in ${Csv}: nothing to trend." -ForegroundColor Yellow; exit 1 }

$names = @($rows[0].PSObject.Properties.Name)
# The time column: the layer's log has t_ms; the outside sampler has elapsed_s.
if ($names -contains 't_ms') {
    $times = @($rows | ForEach-Object { [double]$_.t_ms })
} elseif ($names -contains 'elapsed_s') {
    $times = @($rows | ForEach-Object { [double]$_.elapsed_s * 1000.0 })
} else {
    Write-Host 'No t_ms or elapsed_s column: is this a memory log?' -ForegroundColor Yellow
    exit 1
}

$span = ($times[-1] - $times[0]) / 1000.0
$minutes = [math]::Max($span / 60.0, 0.001)
$step = $span / [math]::Max($rows.Count - 1, 1)
$skip = @('t_ms', 'unix_ms', 'elapsed_s', 'time', 'frame', 'pid')

function Get-SteepestRise([double[]]$Times, [double[]]$Values, [double]$Seconds) {
    $best = 0.0
    $from = 0.0
    $to = 0.0
    $j = 0
    for ($i = 0; $i -lt $Times.Count; $i++) {
        if ($j -lt $i) { $j = $i }
        while ($j + 1 -lt $Times.Count -and ($Times[$j + 1] - $Times[$i]) -le $Seconds * 1000.0) { $j++ }
        $rise = $Values[$j] - $Values[$i]
        if ($rise -gt $best) {
            $best = $rise
            $from = $Times[$i]
            $to = $Times[$j]
        }
    }
    [pscustomobject]@{ Rise = $best; FromS = $from / 1000.0; ToS = $to / 1000.0 }
}

$report = New-Object System.Collections.Generic.List[object]
foreach ($name in $names) {
    if ($skip -contains $name) { continue }
    $values = @($rows | ForEach-Object { [double]$_.$name })
    $rise = Get-SteepestRise $times $values $WindowSeconds
    $report.Add([pscustomobject]@{
            Column      = $name
            First       = $values[0]
            Last        = $values[-1]
            Delta       = $values[-1] - $values[0]
            PerMinute   = ($values[-1] - $values[0]) / $minutes
            Steepest    = $rise.Rise
            SteepestAt  = $rise.FromS
            Relative    = if ($values[0] -ne 0) { ($values[-1] - $values[0]) / [math]::Abs($values[0]) } else { 0 }
        })
}

$rowsLine = "{0} samples over {1:N1} min, {2:N1} s apart" -f $rows.Count, $minutes, $step
Write-Host ''
Write-Host "memory log: $Csv" -ForegroundColor Cyan
Write-Host "  $rowsLine"
Write-Host ''

function Write-Table($Items, [string]$Title) {
    if ($Items.Count -eq 0) { return }
    Write-Host $Title -ForegroundColor Cyan
    Write-Host ('  {0,-26} {1,10} {2,10} {3,10} {4,9} {5,12}' -f 'column', 'first', 'last', 'change', '/min', "steepest ${WindowSeconds}s")
    foreach ($item in $Items) {
        Write-Host ('  {0,-26} {1,10:N0} {2,10:N0} {3,10:N0} {4,9:N1} {5,12:N0}' -f `
                $item.Column, $item.First, $item.Last, $item.Delta, $item.PerMinute, $item.Steepest)
    }
    Write-Host ''
}

$grew = @($report | Where-Object { $_.Delta -gt 0 } | Sort-Object Delta -Descending | Select-Object -First $Top)
$shrank = @($report | Where-Object { $_.Delta -lt 0 } | Sort-Object Delta | Select-Object -First ([math]::Min(6, $Top)))
Write-Table $grew 'what grew, from the first sample to the last:'
Write-Table $shrank 'what shrank:'

# The columns that grew most in proportion to where they started: a small
# category doubling is a leak in the making even when its megabytes are few.
$relative = @($report | Where-Object { $_.First -ge 1 -and $_.Delta -gt 0 -and $_.Relative -gt 0.25 } |
        Sort-Object Relative -Descending | Select-Object -First 8)
if ($relative.Count -gt 0) {
    Write-Host 'grew by a quarter or more, whatever their size:' -ForegroundColor Cyan
    foreach ($item in $relative) {
        Write-Host ('  {0,-26} {1,9:N0} -> {2,9:N0}  ({3:P0})' -f $item.Column, $item.First, $item.Last, $item.Relative)
    }
    Write-Host ''
}

# A timeline: ten or so lines over the run, the columns that say the most about
# where the memory is.
# The outer sampler names the working set `working_set_mb`; the layer `working_mb`.
$workingColumn = if ($names -contains 'working_set_mb') { 'working_set_mb' } else { 'working_mb' }
$timeline = @('private_mb', $workingColumn, 'vram_local_used_mb', 'vram_shared_used_mb',
    'own_alloc_mb', 'view_shapes', 'host_lists_alive', 'texture_pool_mb') |
    Where-Object { $names -contains $_ }
if ($timeline.Count -gt 0) {
    $lines = [math]::Min(12, $rows.Count)
    Write-Host ('the run, sample by sample (every {0:N0}th of {1}):' -f ([math]::Max([math]::Floor($rows.Count / $lines), 1)), $rows.Count) -ForegroundColor Cyan
    Write-Host ('  {0,8} {1,7} {2,9} {3,9} {4,10} {5,10} {6,9} {7,11} {8,8}' -f 'min', 'frame', 'private', 'working', 'vram', 'vramPC', 'own', 'shapes', 'lists')
    $stride = [math]::Max([math]::Floor($rows.Count / $lines), 1)
    for ($i = 0; $i -lt $rows.Count; $i += $stride) {
        $row = $rows[$i]
        $get = { param($n) if ($names -contains $n) { [double]$row.$n } else { 0 } }
        Write-Host ('  {0,8:N1} {1,7:N0} {2,9:N0} {3,9:N0} {4,10:N0} {5,10:N0} {6,9:N0} {7,11:N0} {8,8:N0}' -f `
                (($times[$i] - $times[0]) / 60000.0), (& $get 'frame'), (& $get 'private_mb'), (& $get $workingColumn), `
                (& $get 'vram_local_used_mb'), (& $get 'vram_shared_used_mb'), (& $get 'own_alloc_mb'), `
                (& $get 'view_shapes'), (& $get 'host_lists_alive'))
    }
    Write-Host ''
}

# The rate the run cannot keep: what it would cost in an hour at the pace of
# the last third, where growth there is faster than over the whole run.
$tail = $report | Where-Object { $_.Column -eq 'private_mb' }
if ($tail) {
    Write-Host ("private memory: {0:N0} -> {1:N0} MB over {2:N1} min ({3:N1} MB/min)" -f `
            $tail[0].First, $tail[0].Last, $minutes, $tail[0].PerMinute)
    if ($tail[0].PerMinute -gt 0) {
        Write-Host ("  at that pace an hour costs {0:N0} MB; the steepest {1:N0} s rose {2:N0} MB" -f `
                ($tail[0].PerMinute * 60), $WindowSeconds, $tail[0].Steepest)
    }
}
