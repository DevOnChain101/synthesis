# Synthesis installer for Windows (PowerShell 7+; Windows PowerShell 5.1 is not supported). No admin rights: installs to %LOCALAPPDATA%\Programs\Synthesis.
#   $env:SYNTHESIS_VERSION, $env:SYNTHESIS_INSTALL_DIR, $env:SYNTHESIS_BIN_NAME (default synth),
#   $env:SYNTHESIS_REPO (owner/repo; default DevOnChain101/nemo-acd, kept equal to packaging/REPO by scripts/check-placeholders.mjs),
#   (default) with gh present a failing `gh attestation verify` of SHA256SUMS (signer workflow + source ref v$version) aborts; without gh only the checksum is checked (warning),
#   $env:SYNTHESIS_SKIP_ATTESTATION=1 (skip the attestation check even when gh exists; prints a warning),
#   $env:SYNTHESIS_DOWNLOAD_BASE (mirrors, tests; must be https:// unless $env:SYNTHESIS_INSECURE_DOWNLOAD_BASE_I_KNOW=1)
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion.Major -lt 7) { throw "PowerShell 7+ is required (this is $($PSVersionTable.PSVersion)). Install it with: winget install Microsoft.PowerShell, then run this script with pwsh" }
$repo = if ($env:SYNTHESIS_REPO) { $env:SYNTHESIS_REPO } else { 'DevOnChain101/nemo-acd' }
if ($repo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') { throw "bad repository '$repo' (expected owner/repo)" }
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
if ($env:SYNTHESIS_DOWNLOAD_BASE -and $env:SYNTHESIS_DOWNLOAD_BASE -notmatch '^https://' -and $env:SYNTHESIS_INSECURE_DOWNLOAD_BASE_I_KNOW -ne '1') {
  throw "SYNTHESIS_DOWNLOAD_BASE must be https:// (got '$($env:SYNTHESIS_DOWNLOAD_BASE)')"
}
$dir = if ($env:SYNTHESIS_INSTALL_DIR) { $env:SYNTHESIS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\Synthesis' }
$name = if ($env:SYNTHESIS_BIN_NAME) { $env:SYNTHESIS_BIN_NAME } else { 'synth' }

$arch = switch ($env:PROCESSOR_ARCHITECTURE) { 'AMD64' { 'x86_64' } 'ARM64' { 'aarch64' } default { throw "unsupported CPU $($env:PROCESSOR_ARCHITECTURE)" } }
$target = "$arch-pc-windows-msvc"

$version = $env:SYNTHESIS_VERSION
if (-not $version) {
  $r = Invoke-WebRequest -Uri "https://github.com/$repo/releases/latest" -MaximumRedirection 0 -SkipHttpErrorCheck -ErrorAction SilentlyContinue
  $version = ($r.Headers.Location -split '/v')[-1]
  if (-not $version) { throw 'cannot resolve latest release' }
}
$version = $version.TrimStart('v')
if ($version -notmatch '^[0-9A-Za-z.+-]+$') { throw "bad version '$version' (allowed: 0-9 A-Z a-z . + -)" }
$base = if ($env:SYNTHESIS_DOWNLOAD_BASE) { $env:SYNTHESIS_DOWNLOAD_BASE } else { "https://github.com/$repo/releases/download/v$version" }
$archive = "synthesis-$version-$target.zip"

$tmp = Join-Path ([IO.Path]::GetTempPath()) ("synthesis-" + [guid]::NewGuid())
New-Item -ItemType Directory $tmp | Out-Null
try {
  Write-Host "Downloading $archive"
  Invoke-WebRequest -Uri "$base/$archive" -OutFile (Join-Path $tmp $archive)
  Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
  $line = Get-Content (Join-Path $tmp 'SHA256SUMS') | Where-Object { $_ -match [regex]::Escape("  $archive") + '$' } | Select-Object -First 1
  if (-not $line) { throw "$archive not listed in SHA256SUMS" }
  $want = ($line -split '\s+')[0].ToLower()
  $got = (Get-FileHash (Join-Path $tmp $archive) -Algorithm SHA256).Hash.ToLower()
  if ($want -ne $got) { throw "checksum mismatch for $archive (expected $want, got $got)" }
  Write-Host 'sha256 ok'
  # SHA256SUMS is only as trustworthy as its source: verify its build-provenance attestation (made by release.yml).
  if ($env:SYNTHESIS_SKIP_ATTESTATION -eq '1') {
    Write-Warning 'SYNTHESIS_SKIP_ATTESTATION=1: SHA256SUMS was NOT authenticated; only the archive hash against it was checked. Anyone who can alter the download can alter both.'
  } elseif (Get-Command gh -ErrorAction SilentlyContinue) {
    gh attestation verify (Join-Path $tmp 'SHA256SUMS') --repo $repo --signer-workflow "$repo/.github/workflows/release.yml" --source-ref "refs/tags/v$version"
    if ($LASTEXITCODE -ne 0) { throw 'attestation verification of SHA256SUMS failed (not logged in to gh? set SYNTHESIS_SKIP_ATTESTATION=1 to accept checksum-only)' }
    Write-Host 'attestation ok'
  } else {
    Write-Warning 'gh (GitHub CLI) not found: SHA256SUMS was NOT authenticated, only the archive hash against it was checked. Install gh (https://cli.github.com), run gh auth login and re-run for full verification.'
  }
  Expand-Archive (Join-Path $tmp $archive) -DestinationPath $tmp
  $src = Join-Path $tmp "synthesis-$version-$target"
  New-Item -ItemType Directory -Force $dir | Out-Null
  Copy-Item (Join-Path $src 'synth.exe') (Join-Path $dir "$name.exe") -Force
  Write-Host "Installed $(Join-Path $dir "$name.exe") ($version, $target)"
  & (Join-Path $dir "$name.exe") doctor
  if (($env:PATH -split ';') -notcontains $dir) {
    Write-Host "Add $dir to your user PATH (then open a new terminal), for example:"
    Write-Host "  [Environment]::SetEnvironmentVariable('Path', [Environment]::GetEnvironmentVariable('Path','User') + ';$dir', 'User')"
  }
  $other = Get-Command $name -ErrorAction SilentlyContinue
  if ($other -and $other.Source -ne (Join-Path $dir "$name.exe")) { Write-Warning "'$name' on PATH resolves to $($other.Source) (Node CLI?). Put $dir first or set SYNTHESIS_BIN_NAME." }
} finally { Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue }
