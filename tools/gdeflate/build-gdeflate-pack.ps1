<#
.SYNOPSIS
  Builds the GDeflate bridge and converts the title's texture archives into a
  verified DirectStorage GDeflate pack (FX1GDF01).

.DESCRIPTION
  Runs the whole offline pipeline:
    1. Build fx1_bc.dll       (BCPack decode helper; rustc against victory-formats)
    2. Build fx1-storage.dll  (DirectStorage bridge; cl.exe + DirectStorage SDK)
    3. repack.py              (decode the original zlib/BCPack streams, re-encode GDeflate)
    4. publish.py             (merge the sidecars into one FX1GDF01 pack)
    5. verify-pack.py         (decode one block per codec on a real D3D12 device)

  Nothing is committed; the pack, sidecars, and bridge DLLs stay outside the
  checkout. Run this from anywhere; it locates the repo from the script path.

.PARAMETER Root
  The archive root holding the title's *.ttpf (and *.rpf) files.
.PARAMETER Indexes
  Optional folder of *.json RPF stream indexes (SixNG). Omit to skip RPF.
.PARAMETER Sdk
  A DirectStorage SDK folder containing dstorage.dll and dstoragecore.dll
  (and, to build the bridge, native/include/dstorage.h and
  native/lib/x64/dstorage.lib).
.PARAMETER Bridge
  A writable folder (outside the checkout) for fx1_bc.dll and fx1-storage.dll.
.PARAMETER Sidecars
  A writable folder for the per-source SQLite sidecars.
.PARAMETER Pack
  The output pack path (FX1GDF01), e.g. D:\fx1\indexed.gdeflate.
.PARAMETER Encoder
  directstorage (default) or nvcomp (needs CUDA + the nvcomp Python bindings).
.PARAMETER CompressionLevel
  fastest | default | best-ratio (DirectStorage encoder only).
.PARAMETER Workers
  1-8 encoder workers (nvcomp only).
.PARAMETER SkipBridge
  Skip building fx1-storage.dll (e.g. no MSVC). repack + publish still run;
  verify-pack is skipped.

.EXAMPLE
  .\tools\gdeflate\build-gdeflate-pack.ps1 -Root D:\game -Indexes D:\game-index `
    -Sdk D:\sdk\directstorage -Bridge D:\fx1 -Sidecars D:\fx1-sidecars -Pack D:\fx1\indexed.gdeflate
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Root,
    [Parameter(Mandatory = $true)][string]$Sdk,
    [Parameter(Mandatory = $true)][string]$Bridge,
    [Parameter(Mandatory = $true)][string]$Sidecars,
    [Parameter(Mandatory = $true)][string]$Pack,
    [string]$Indexes = "",
    [ValidateSet("directstorage", "nvcomp")][string]$Encoder = "directstorage",
    [ValidateSet("fastest", "default", "best-ratio")][string]$CompressionLevel = "best-ratio",
    [ValidateRange(1, 8)][int]$Workers = 2,
    [switch]$SkipBridge
)

$ErrorActionPreference = "Stop"

$ToolsDir = Split-Path -Parent $MyInvocation.MyCommand.Path   # .../tools/gdeflate
$Repo     = (Resolve-Path (Join-Path $ToolsDir "..\..")).Path

function Step([string]$msg) { Write-Host ""; Write-Host "==> $msg" -ForegroundColor Cyan }
function Die([string]$msg)  { Write-Error "GDeflate build failed: $msg"; exit 1 }

# Builds fx1-storage.dll with cl.exe (either already on PATH, or after running vcvars64.bat).
function Build-Bridge([string]$sdk, [string]$bridge, [string]$toolsDir, [string]$outDll, [string]$vcvars = "") {
    $dsInclude = Join-Path $sdk "native\include"
    $dsLib     = Join-Path $sdk "native\lib\x64"
    if (-not (Test-Path -LiteralPath (Join-Path $dsInclude "dstorage.h"))) { Die "missing $dsInclude\dstorage.h" }
    if (-not (Test-Path -LiteralPath (Join-Path $dsLib "dstorage.lib")))   { Die "missing $dsLib\dstorage.lib" }
    $obj   = Join-Path $bridge "runtime.obj"
    $cpp   = Join-Path $toolsDir "runtime.cpp"
    $clCmd = 'cl /nologo /LD /EHsc /std:c++20 /O2 /I"{0}" /Fo"{1}" "{2}" /Fe"{3}" /link /LIBPATH:"{4}" dstorage.lib d3d12.lib dxgi.lib bcrypt.lib' -f $dsInclude, $obj, $cpp, $outDll, $dsLib
    if ($vcvars) { $cmd = 'call "{0}" x64 >nul 2>&1 && {1}' -f $vcvars, $clCmd } else { $cmd = $clCmd }
    cmd /c $cmd
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $outDll)) { Die "cl fx1-storage.dll failed" }
    Write-Host "built $outDll"
}

# Locates vcvars64.bat in the standard Visual Studio install paths.
function Find-VcVars {
    foreach ($root in @("${env:ProgramFiles}\Microsoft Visual Studio", "${env:ProgramFiles(x86)}\Microsoft Visual Studio")) {
        if (Test-Path -LiteralPath $root) {
            $found = Get-ChildItem -Path $root -Recurse -Filter "vcvars64.bat" -ErrorAction SilentlyContinue |
                     Select-Object -First 1 -ExpandProperty FullName
            if ($found) { return $found }
        }
    }
    return $null
}

# ---------------------------------------------------------------------------
Step "Checking prerequisites"
# ---------------------------------------------------------------------------
if (-not (Get-Command python -ErrorAction SilentlyContinue)) { Die "python not on PATH" }
if (-not (Get-Command cargo  -ErrorAction SilentlyContinue)) { Die "cargo not on PATH (rust-toolchain.toml inactive?)" }
if (-not (Get-Command rustc  -ErrorAction SilentlyContinue)) { Die "rustc not on PATH" }
foreach ($d in @($Root, $Sdk)) {
    if (-not (Test-Path -LiteralPath $d)) { Die "missing directory: $d" }
}
New-Item -ItemType Directory -Path $Bridge, $Sidecars -Force | Out-Null

$dsCore = Join-Path $Sdk "dstoragecore.dll"
$dsDll  = Join-Path $Sdk "dstorage.dll"
if (-not (Test-Path -LiteralPath $dsCore) -or -not (Test-Path -LiteralPath $dsDll)) {
    Die "DirectStorage runtime DLLs missing from '$Sdk' (need dstoragecore.dll and dstorage.dll)"
}

# ---------------------------------------------------------------------------
Step "Building fx1_bc.dll (BCPack decode helper)"
# ---------------------------------------------------------------------------
$formatsRlib = "target\release\libvictory_formats.rlib"   # relative to $Repo
$depsDir     = "target\release\deps"
$bcDll       = Join-Path $Bridge "fx1_bc.dll"

Push-Location $Repo
try {
    & cargo build --release --locked -p victory-formats
    if ($LASTEXITCODE -ne 0) { Die "cargo build victory-formats failed" }
    if (-not (Test-Path -LiteralPath $formatsRlib)) { Die "victory-formats rlib not produced: $formatsRlib" }
    # rustc must run inside the repo so the rustup shim honours rust-toolchain.toml (1.98.1).
    & rustc --crate-name fx1_bc --edition 2024 --crate-type cdylib `
        -L "dependency=$depsDir" --extern "victory_formats=$formatsRlib" `
        (Join-Path $ToolsDir "bc_decode.rs") -o $bcDll
    if ($LASTEXITCODE -ne 0) { Die "rustc fx1_bc.dll failed" }
} finally { Pop-Location }
Write-Host "built $bcDll"

# ---------------------------------------------------------------------------
Step "Building fx1-storage.dll (DirectStorage bridge)"
# ---------------------------------------------------------------------------
$storageDll = Join-Path $Bridge "fx1-storage.dll"
if ($SkipBridge) {
    Write-Warning "fx1-storage.dll skipped; verify-pack will be skipped."
} elseif (Get-Command cl -ErrorAction SilentlyContinue) {
    Build-Bridge $Sdk $Bridge $ToolsDir $storageDll
} else {
    $vcvars = Find-VcVars
    if ($vcvars) {
        Build-Bridge $Sdk $Bridge $ToolsDir $storageDll $vcvars
    } else {
        Write-Warning "cl.exe not found and no vcvars64.bat located; skipping fx1-storage.dll. Install the 'Desktop development with C++' workload or use -SkipBridge."
    }
}

# ---------------------------------------------------------------------------
Step "Re-encoding to GDeflate (repack.py -> publish.py)"
# ---------------------------------------------------------------------------
$repackArgs = @(
    "-B", (Join-Path $ToolsDir "repack.py"),
    "--root", $Root, "--output", $Sidecars, "--sdk", $Sdk, "--bc", $Bridge,
    "--encoder", $Encoder, "--workers", "$Workers"
)
if ($Indexes) { $repackArgs += @("--indexes", $Indexes) }
if ($Encoder -eq "directstorage") { $repackArgs += @("--compression-level", $CompressionLevel) }
& python @repackArgs
if ($LASTEXITCODE -ne 0) { Die "repack.py failed" }

& python -B (Join-Path $ToolsDir "publish.py") --root $Root --sidecars $Sidecars --out $Pack
if ($LASTEXITCODE -ne 0) { Die "publish.py failed" }

# ---------------------------------------------------------------------------
Step "Verifying on the adapter (verify-pack.py)"
# ---------------------------------------------------------------------------
if (-not $SkipBridge -and (Test-Path -LiteralPath $storageDll)) {
    # verify-pack.py loads fx1-storage.dll + dstoragecore.dll from the pack's folder.
    $packDir = Split-Path -Parent $Pack
    if (-not $packDir) { $packDir = "." }
    New-Item -ItemType Directory -Path $packDir -Force | Out-Null
    Copy-Item -LiteralPath $storageDll -Destination (Join-Path $packDir "fx1-storage.dll") -Force
    Copy-Item -LiteralPath $dsCore     -Destination (Join-Path $packDir "dstoragecore.dll") -Force
    & python -B (Join-Path $ToolsDir "verify-pack.py") --root $Root --sidecars $Sidecars --pack $Pack
    if ($LASTEXITCODE -ne 0) { Die "verify-pack.py failed" }
} else {
    Write-Warning "Skipping verify-pack.py (fx1-storage.dll not built)."
}

Step "Done"
Write-Host "Pack:   $Pack"
Write-Host "Bridge: $Bridge"
Write-Host ""
Write-Host "Run with:"
Write-Host "  `$env:VICTORY_GDEFLATE_PACK='$Pack'"
Write-Host "  `$env:VICTORY_GDEFLATE_BRIDGE_DIR='$Bridge'"
