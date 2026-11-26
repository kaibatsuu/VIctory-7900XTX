param(
    [Parameter(Mandatory)][string]$DirectStorage,
    [Parameter(Mandatory)][string]$BcCodec,
    [Parameter(Mandatory)][string]$VsDevCmd
)

$ErrorActionPreference = 'Stop'
$DirectStorage = (Resolve-Path -LiteralPath $DirectStorage).Path
$BcCodec = (Resolve-Path -LiteralPath $BcCodec).Path
$VsDevCmd = (Resolve-Path -LiteralPath $VsDevCmd).Path
$out = Join-Path $PSScriptRoot '..\..\target\offline-feedback'
$out = (New-Item -ItemType Directory -Path $out -Force).FullName
Copy-Item -LiteralPath @("$DirectStorage\dstorage.dll", "$DirectStorage\dstoragecore.dll") -Destination $out

function Compile-Shader($source, $output, [string[]]$defines = @()) {
    & dxc -T cs_6_5 -E main -WX @defines -Fo "$out\$output.dxil" "$PSScriptRoot\$source.hlsl"
    if ($LASTEXITCODE -ne 0) { throw "DXC failed for $output" }
}

Compile-Shader feedback feedback
Compile-Shader feedback diagonal @('-DFX1_DIAGONAL_TILES')
Compile-Shader compact-feedback compact-feedback
Compile-Shader sample-streamed-tile sample

$build = Join-Path $out 'compile.cmd'
@"
@echo off
call "$VsDevCmd" -arch=x64 -host_arch=x64 >nul || exit /b 1
cl /nologo /EHsc /std:c++20 /O2 /I "$DirectStorage\sdk\native\include" /Fo:"$out\streaming-test.obj" "$PSScriptRoot\streaming-test.cpp" /Fe:"$out\streaming-test.exe" /link /LIBPATH:"$DirectStorage\sdk\native\lib\x64" dstorage.lib d3d12.lib dxgi.lib bcrypt.lib
"@ | Set-Content -LiteralPath $build -Encoding ascii
& $build
if ($LASTEXITCODE -ne 0) { throw 'Sampler Feedback fixture build failed' }

& python -B "$PSScriptRoot\test_reserved_tiles.py" --sdk $out --bc $BcCodec
if ($LASTEXITCODE -ne 0) { throw 'Sampler Feedback hardware checks failed' }
