# Fetch the tailcat (custom mesh tunnel) binary into res/tailcat/.
# Called by the build when res/tailcat/tailcat.exe is missing.
# tailcat v0.7.0 windows_amd64, sha256 from the official release checksums.
$ErrorActionPreference = 'Stop'
$ver = '0.7.0'
$sha256 = 'f04cac07e3bf6c700b0b543df0d3315a3b312cd5a05ee76714e1cc848e9b2dff'
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe = Join-Path $dir 'tailcat.exe'
if (Test-Path $exe) { Write-Output "tailcat.exe already present"; exit 0 }

$tmp = Join-Path $env:TEMP "tailcat_${ver}_windows_amd64.zip"
$url = "https://github.com/tailscale/tailcat/releases/download/v${ver}/tailcat_${ver}_windows_amd64.zip"
$curl = "$env:SystemRoot\System32\curl.exe"
# github.com is not directly reachable from this network; go through the mooncn
# mirror, fall back to gh-proxy.
$enc = [Uri]::EscapeDataString($url)
& $curl -L --fail -sS --retry 2 --connect-timeout 20 --max-time 600 -o $tmp "https://faster.mooncn.win/?url=$enc"
if (-not (Test-Path $tmp)) {
    & $curl -L --fail -sS --retry 2 --connect-timeout 20 --max-time 600 -o $tmp "https://gh-proxy.com/$url"
}
if (-not (Test-Path $tmp)) { Write-Error "download failed: $url"; exit 1 }

$h = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
if ($h -ne $sha256) { Write-Error "sha256 mismatch: $h"; exit 1 }

Expand-Archive -Path $tmp -DestinationPath "$dir\.tmp-unpack" -Force
Copy-Item (Join-Path $dir ".tmp-unpack\tailcat.exe") $exe -Force
Remove-Item (Join-Path $dir '.tmp-unpack') -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $tmp -Force -ErrorAction SilentlyContinue
Write-Output "tailcat.exe ${ver} installed at $exe"
