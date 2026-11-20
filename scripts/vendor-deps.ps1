<#
.SYNOPSIS
    按 src-tauri/Cargo.lock 把全部 crates.io 依赖下载并解包成 vendor 目录（离线构建用）。

.DESCRIPTION
    本机 cargo 自带的 libcurl + Schannel 无法完成 TLS 握手（直接连 crates.io / rsproxy
    都报 [35] SSL connect error），cargo 自己下不了依赖。这个脚本绕开它：
    用系统 curl / .NET HttpClient 下载 .crate，用 tar 解包，再补 .cargo-checksum.json。

    生成后由 src-tauri/.cargo/config.toml 做源替换：
        [source.crates-io]
        replace-with = "vendored-sources"
        [source.vendored-sources]
        directory = "<VendorDir>"

.EXAMPLE
    # 默认解到 <仓库根>/vendor
    powershell -ExecutionPolicy Bypass -File scripts/vendor-deps.ps1

.EXAMPLE
    # 与本仓库当前 config.toml 一致的位置
    powershell -ExecutionPolicy Bypass -File scripts/vendor-deps.ps1 -VendorDir C:\cargo-home\novel-overlay-vendor
#>
param(
    [string]$VendorDir,
    [string]$CacheDir,
    [int]$Parallel = 12
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Net.Http

$repo = Split-Path -Parent $PSScriptRoot
if (-not $VendorDir) { $VendorDir = Join-Path $repo 'vendor' }
if (-not $CacheDir) { $CacheDir = Join-Path $env:TEMP 'novel-overlay-crates' }
$lockPath = Join-Path $repo 'src-tauri/Cargo.lock'

New-Item -ItemType Directory -Force -Path $VendorDir, $CacheDir | Out-Null

# ---- 解析 Cargo.lock ----
$text = [System.IO.File]::ReadAllText($lockPath)
$blocks = [regex]::Split($text, '(?m)^\[\[package\]\]\s*$')
$pkgs = New-Object System.Collections.ArrayList
foreach ($block in $blocks) {
    if ($block -notmatch 'source = "registry\+') { continue }
    $name = [regex]::Match($block, '(?m)^name = "([^"]+)"').Groups[1].Value
    $ver = [regex]::Match($block, '(?m)^version = "([^"]+)"').Groups[1].Value
    $sum = [regex]::Match($block, '(?m)^checksum = "([^"]+)"').Groups[1].Value
    if ($name -and $ver) {
        [void]$pkgs.Add([pscustomobject]@{ Name = $name; Version = $ver; Checksum = $sum })
    }
}
Write-Host "crates.io 依赖: $($pkgs.Count) 个"

# ---- 下载（.NET HttpClient 并发；curl 单线程太慢）----
$pending = @()
foreach ($p in $pkgs) {
    $file = Join-Path $CacheDir ("{0}-{1}.crate" -f $p.Name, $p.Version)
    if ((Test-Path $file) -and ((Get-Item $file).Length -gt 0)) { continue }
    $pending += $p
}
Write-Host "需要下载: $($pending.Count) 个"

$handler = New-Object System.Net.Http.HttpClientHandler
$handler.MaxConnectionsPerServer = 16
$client = New-Object System.Net.Http.HttpClient($handler)
$client.Timeout = [TimeSpan]::FromSeconds(180)
$client.DefaultRequestHeaders.Add('User-Agent', 'novel-overlay-vendor/1.0')

$done = 0
for ($i = 0; $i -lt $pending.Count; $i += $Parallel) {
    $end = [Math]::Min($i + $Parallel - 1, $pending.Count - 1)
    $batch = $pending[$i..$end]
    $tasks = New-Object System.Collections.ArrayList
    foreach ($p in $batch) {
        [void]$tasks.Add($client.GetByteArrayAsync(
            ("https://static.crates.io/crates/{0}/{0}-{1}.crate" -f $p.Name, $p.Version)))
    }
    try { [System.Threading.Tasks.Task]::WaitAll([System.Threading.Tasks.Task[]]$tasks.ToArray()) } catch {}
    for ($k = 0; $k -lt $batch.Count; $k++) {
        $p = $batch[$k]
        $file = Join-Path $CacheDir ("{0}-{1}.crate" -f $p.Name, $p.Version)
        try {
            $bytes = $tasks[$k].Result
            if ($bytes -and $bytes.Length -gt 0) { [System.IO.File]::WriteAllBytes($file, $bytes) }
        } catch {
            Write-Warning ("下载失败 {0}-{1}: {2}" -f $p.Name, $p.Version, $_.Exception.Message)
        }
    }
    $done += $batch.Count
    if ($done % 60 -lt $Parallel) { Write-Host "  已处理 $done / $($pending.Count)" }
}

# ---- 解包 + 写校验文件 ----
$extracted = 0
$missing = @()
foreach ($p in $pkgs) {
    $file = Join-Path $CacheDir ("{0}-{1}.crate" -f $p.Name, $p.Version)
    if (-not (Test-Path $file)) { $missing += "$($p.Name)-$($p.Version)"; continue }
    $dir = Join-Path $VendorDir ("{0}-{1}" -f $p.Name, $p.Version)
    if (-not (Test-Path $dir)) {
        & tar.exe -xzf $file -C $VendorDir
        if ($LASTEXITCODE -ne 0) { Write-Warning "解包失败: $file"; continue }
    }
    [System.IO.File]::WriteAllText(
        (Join-Path $dir '.cargo-checksum.json'),
        '{"files":{},"package":"' + $p.Checksum + '"}')
    $extracted++
}

Write-Host "已就绪: $extracted / $($pkgs.Count) → $VendorDir"
if ($missing.Count -gt 0) {
    Write-Warning "缺失 $($missing.Count) 个（重跑脚本会续传）：$($missing -join ', ')"
    exit 1
}
