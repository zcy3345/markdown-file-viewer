# scripts/build-release.ps1
# 产出「可分发」的 exe：把编译期路径重映射，避免把本机的用户名/目录写进二进制。
#   依赖 crate 的 panic 位置默认形如  C:\Users\<你>\.cargo\registry\src\...\lib.rs
#   本脚本把它重映射成 /cargo-registry，把仓库路径重映射成 .
#
# 用法： powershell -ExecutionPolicy Bypass -File scripts\build-release.ps1
$ErrorActionPreference = 'Stop'

$repo  = Split-Path -Parent $PSScriptRoot
$cargo = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }

# 用 CARGO_ENCODED_RUSTFLAGS（\x1f 分隔）而不是 RUSTFLAGS，路径里有空格也不会被拆开
$sep = [char]31
$env:CARGO_ENCODED_RUSTFLAGS = (@(
    "--remap-path-prefix=$cargo\registry=/cargo-registry",
    "--remap-path-prefix=$repo=."
) -join $sep)

Push-Location (Join-Path $repo 'mdv')
try {
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw 'cargo build 失败' }
} finally {
    Pop-Location
}

$exe = Join-Path $repo 'mdv\target\release\mdv.exe'
Copy-Item $exe (Join-Path $repo 'mdv.exe') -Force
$size = (Get-Item $exe).Length
Write-Host ("产物: {0}  ({1:N0} 字节 / {2:N2} MiB)" -f $exe, $size, ($size / 1MB))

# 自检：确认没有把用户名/仓库路径写进去
$bytes = [IO.File]::ReadAllBytes($exe)
# 针串来自运行时环境，不写死本机路径/用户名
$needles = @($env:USERNAME, $env:COMPUTERNAME, (Split-Path $repo -Leaf), '.cargo', 'AppData')
$bad = 0
foreach ($n in $needles) {
    if (-not $n) { continue }
    foreach ($enc in @([Text.Encoding]::ASCII, [Text.Encoding]::Unicode)) {
        $pat = $enc.GetBytes($n)
        if ([Text.Encoding]::ASCII.GetString($bytes).Contains($n)) {
            Write-Host "  ⚠ 二进制里仍含 '$n'" -ForegroundColor Yellow
            $bad++
        }
        break
    }
}
if ($bad -eq 0) { Write-Host '  ✓ 未检出用户名 / 计算机名 / 仓库路径' -ForegroundColor Green }
