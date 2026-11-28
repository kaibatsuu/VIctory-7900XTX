# Starts the title with the settings this fork is played with (README.md, "How to run it").
#   scripts\play.ps1 -Exe <path of your own copy's executable>
# -Safe         without the newer, faster ways (the first thing to try when something is wrong)
# -RayTracing   the title's own ray-traced indirect light: lit interiors, about a third slower
# -Fsr4K        the 4K output with the title's FSR frontend (quality, balanced or performance); by default the launcher runs the Series X 1440p mode, 2560x1440 rendered 1:1, with no upscaling
# -Console      series-x (default) or series-s
# -Uncap        no frame-rate limit at all (VICTORY_FRAME_INTERVAL_MS=0, VICTORY_TITLE_FPS=0); an experiment, not a playable setting
# -Memory       the memory trims ported from victory-memory-stack (indirect budgets packed, command allocators taken anew)
# -Build        target\diag (the default, the build all measurements are of) or target\release
# -More         other settings: -More @{VICTORY_PANEL='1'}
# Run it from a normal window, not as administrator: the driver then keeps its shader cache elsewhere.
param(
    [Parameter(Mandatory)][string]$Exe,
    [switch]$Safe,
    [switch]$RayTracing,
    [ValidateSet('native', 'quality', 'balanced', 'performance')][string]$Fsr4K,
    [ValidateSet('series-x', 'series-s')][string]$Console = 'series-x',
    [switch]$Uncap,
    [switch]$Memory,
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [hashtable]$More = @{}
)
$settings = @{
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_CARD_REHASH_RUNS = '1'
    VICTORY_JOIN_COPIES = '1'; VICTORY_INDIRECT_WINDOW = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_TITLE_PRIORITIES = '0'; VICTORY_CARD_AWAKE = '1'
    VICTORY_SHOW_AFTER = '1'; VICTORY_AMBIENT = '0.8'; VICTORY_D3D12_DRED = 'light'
    # The Series X's 1440p display mode, rendered 1:1: the window and the scene are 2560x1440,
    # with no upscaling (the title's own temporal resolve at 1.0), and the HUD scales with the
    # output height. The FSR 4K profile patches the output size itself, so it stays off here.
    # `-Fsr4K quality` (or balanced/performance) goes back to the 4K output with FSR instead.
    VICTORY_OUTPUT = '2560x1440'; VICTORY_RENDER_SCALE = '1.0'; VICTORY_FSR_4K = '0'
}
if ($Safe) {
    $settings.VICTORY_UNDER_WIDE = '0'; $settings.VICTORY_LAND_ECHO = '0'
} else {
    $settings.VICTORY_THIN = '1'; $settings.VICTORY_JOIN_PARTS = '8'; $settings.VICTORY_UPLOAD_AHEAD = '1'
}
if ($Memory) {
    $settings.VICTORY_BUDGET_PACK = '1'      # a list's indirect budget is a 256-byte slot of a shared 64 KiB buffer
    $settings.VICTORY_ALLOCATOR_TRIM = '1'   # a list's command allocator is taken anew at a reset every 30 s
}
if ($RayTracing) { $settings.VICTORY_BVH_BUILDER = '1' }
if ($Fsr4K) { $settings.VICTORY_FSR_4K = $Fsr4K.ToLowerInvariant() }
if ($Uncap) {
    $settings.VICTORY_FRAME_INTERVAL_MS = '0'   # our frame clock never waits
    $settings.VICTORY_TITLE_FPS = '0'           # the title's rate, limiter and shortest sim step: no limit
}
foreach ($k in $More.Keys) { $settings[$k] = $More[$k] }

$victory = Join-Path $PSScriptRoot "..\target\$Build\victory.exe"
if (-not (Test-Path $victory)) { Write-Host "No $victory. Build it first (README.md)."; exit 1 }
if (-not (Test-Path $Exe)) { Write-Host "No $Exe."; exit 1 }
# (Other settings left over in this window go; where your files are stays.)
$paths = 'VICTORY_CONFIG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_FIDELITYFX_DIR'
$inherited = @(Get-ChildItem Env:VICTORY_* | Where-Object { $_.Name -notin $paths })
foreach ($entry in $inherited) { Remove-Item "Env:$($entry.Name)" }
foreach ($k in $settings.Keys) { Set-Item "Env:$k" $settings[$k] }
$discarded = $inherited | Where-Object {
    $_.Name -notin $More.Keys -and -not $_.Value.Equals([Environment]::GetEnvironmentVariable($_.Name))
} | Sort-Object Name
if ($discarded) {
    Write-Host "Discarded inherited values for $($discarded.Name -join ', '). Use -More to pass them explicitly."
}
$Exe = (Resolve-Path $Exe).Path
Push-Location (Join-Path $PSScriptRoot '..')   # victory.toml is looked for here
try { & $victory run $Exe --execute --console $Console } finally { Pop-Location }
