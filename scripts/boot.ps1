# Build, warm and launch this checkout of the title.
#   scripts\boot.ps1                   build -> pre-launch warm -> play
#   scripts\boot.ps1 -Book             first a recorded run, then `victory warm --used`, then play
#   scripts\boot.ps1 -WarmShaders      force a full shaders.rpf translation before launch
#   scripts\boot.ps1 -SkipBuild -Uncap use the exe already built, no cap
#   scripts\boot.ps1 -NoIsolate        compile pipelines in the game's own process (a driver crash ends the game)
# The game runs with VICTORY_PIPELINE_ISOLATE=1, as the README says: a pipeline that crashes
# AMD's compiler costs a child process, not the run.
# If PowerShell refuses to run this file, use scripts\boot.cmd (bypass for this process only).
param(
    [string]$GameDir = 'C:\Games\GTA\GTA6.Xbox.v1.0.2',
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [switch]$SkipBuild,
    # Force `victory shaders <game>\shaders.rpf` (the launch itself warms only what changed).
    [switch]$WarmShaders,
    # Set VICTORY_WARM_ON_LAUNCH=0 for the runs of this boot.
    [switch]$SkipWarm,
    # Run once with VICTORY_PIPELINE_BOOK=1, then `victory warm --used`.
    [switch]$Book,
    [switch]$Uncap,
    [switch]$Safe,
    [switch]$RayTracing,
    # Build and warm, but do not start the game.
    [switch]$NoLaunch,
    # Leave VICTORY_PIPELINE_ISOLATE off for the game's run.
    [switch]$NoIsolate
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $root

function Fail([string]$message) { throw $message }
function Check-Last([string]$what) {
    if ($LASTEXITCODE -ne 0) { Fail "$what failed (exit $LASTEXITCODE)" }
}

$branch = (git rev-parse --abbrev-ref HEAD).Trim()
$dirty = (git status --porcelain | Measure-Object).Count
Write-Host "boot: branch $branch, $dirty changed or untracked paths, profile $Build" -ForegroundColor Cyan

$exe = Join-Path $root "target\$Build\victory.exe"
$titleExe = Join-Path $GameDir 'game_scarlett_master.exe'
$shaders = Join-Path $GameDir 'shaders.rpf'
if (-not (Test-Path $titleExe)) { Fail "No $titleExe - set -GameDir to your copy's folder." }
if (-not (Test-Path $shaders)) { Write-Host "boot: no $shaders, pre-launch shader warming will skip it" -ForegroundColor Yellow }

if (-not $SkipBuild) {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { Fail 'cargo is not on PATH.' }
    Write-Host "boot: cargo build --locked --profile $Build -p victory --features diagnostics"
    & cargo build --locked --profile $Build -p victory --features diagnostics
    Check-Last 'cargo build'
}
if (-not (Test-Path $exe)) { Fail "No $exe - build first." }

if ($WarmShaders -and (Test-Path $shaders)) {
    Write-Host "boot: victory shaders $shaders"
    & $exe shaders $shaders
    Check-Last 'victory shaders'
}

$play = Join-Path $PSScriptRoot 'play.ps1'
function Invoke-Play {
    param([hashtable]$More = @{}, [switch]$Uncap, [switch]$Safe, [switch]$RayTracing)
    $call = @{ Exe = $titleExe; More = $More }
    if ($Uncap) { $call.Uncap = $true }
    if ($Safe) { $call.Safe = $true }
    if ($RayTracing) { $call.RayTracing = $true }
    & $play @call
}

if ($Book) {
    Write-Host 'boot: recorded run - play a little so the pipelines are written down, then close the game' -ForegroundColor Cyan
    $more = @{ VICTORY_PIPELINE_BOOK = '1'; VICTORY_PIPELINE_ISOLATE = '1' }
    if ($SkipWarm) { $more.VICTORY_WARM_ON_LAUNCH = '0' }
    Invoke-Play -More $more
    Write-Host 'boot: victory warm --used'
    & $exe warm --used
    Check-Last 'victory warm --used'
}

if (-not $NoLaunch) {
    $more = @{}
    if (-not $NoIsolate) { $more.VICTORY_PIPELINE_ISOLATE = '1' }
    if ($SkipWarm) { $more.VICTORY_WARM_ON_LAUNCH = '0' }
    Invoke-Play -More $more -Uncap:$Uncap -Safe:$Safe -RayTracing:$RayTracing
}
