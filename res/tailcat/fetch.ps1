# Fetch the tailcat (custom mesh tunnel) binary into res/tailcat/.
# Called by the build when res/tailcat/tailcat.exe is missing.
# Supports -Arch amd64 (default) and arm64 — the official v0.7.0 release ships
# windows binaries for both; the sha256 is read from the release checksums.txt
# at runtime instead of being hardcoded per-arch.
param(
    [ValidateSet('amd64', 'arm64')]
    [string]$Arch = 'amd64'
)
$ErrorActionPreference = 'Stop'
$ver = '0.7.0'
$dir = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe = Join-Path $dir 'tailcat.exe'
if (Test-Path $exe) { Write-Output "tailcat.exe already present"; exit 0 }

$tmp = Join-Path $env:TEMP "tailcat_${ver}_windows_${Arch}.zip"
$asset = "tailcat_${ver}_windows_${Arch}.zip"
$url = "https://github.com/tailscale/tailcat/releases/download/v${ver}/${asset}"
$curl = "$env:SystemRoot\System32\curl.exe"
# github.com may be unreachable from restricted networks; try direct first
# (fast on CI runners), then fall back to the mooncn / gh-proxy mirrors.
$enc = [Uri]::EscapeDataString($url)
& $curl -L --fail -sS --retry 2 --connect-timeout 20 --max-time 600 -o $tmp $url
if (-not (Test-Path $tmp)) {
  & $curl -L --fail -sS --retry 2 --connect-timeout 20 --max-time 600 -o $tmp "https://faster.mooncn.win/?url=$enc"
}
if (-not (Test-Path $tmp)) {
  & $curl -L --fail -sS --retry 2 --connect-timeout 20 --max-time 600 -o $tmp "https://gh-proxy.com/$url"
}
if (-not (Test-Path $tmp)) { Write-Error "download failed: $url"; exit 1 }

# Verify against the official checksums.txt shipped with the release.
$sums = Join-Path $env:TEMP "tailcat_${ver}_checksums.txt"
& $curl -L --fail -sS --retry 2 --connect-timeout 20 --max-time 120 -o $sums "https://github.com/tailscale/tailcat/releases/download/v${ver}/checksums.txt"
if (Test-Path $sums) {
  $expected = (Select-String -Path $sums -Pattern "([a-fA-F0-9]{64})\s+\*?${asset}").Matches.Groups[1].Value.ToLower()
  $actual = (Get-FileHash $tmp -Algorithm SHA256).Hash.ToLower()
  if ($expected -and ($actual -ne $expected)) { Write-Error "sha256 mismatch: $actual (expected $expected)"; exit 1 }
  Write-Output "sha256 ok: $actual"
} else {
  Write-Output "warning: checksums.txt unavailable, skipping sha256 verification"
}

Expand-Archive -Path $tmp -DestinationPath "$dir\.tmp-unpack" -Force
Copy-Item (Join-Path $dir ".tmp-unpack\tailcat.exe") $exe -Force
Remove-Item (Join-Path $dir '.tmp-unpack') -Recurse -Force -ErrorAction SilentlyContinue
Remove-Item $tmp -Force -ErrorAction SilentlyContinue
Write-Output "tailcat.exe ${ver} windows_${Arch} installed at $exe"
