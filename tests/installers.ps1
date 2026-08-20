# Exercise the Windows installer's independent digest root and no-follow
# replacement boundary against a loopback-only fake release.
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$version = ((Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value)
$tmp = Join-Path ([IO.Path]::GetTempPath()) ("dbmd-installer-test-" + [IO.Path]::GetRandomFileName())
$release = Join-Path $tmp "release\v$version"
$trusted = Join-Path $tmp "trusted\$version"
$installDir = Join-Path $tmp 'install'
New-Item -ItemType Directory -Force -Path $release | Out-Null
New-Item -ItemType Directory -Force -Path $trusted | Out-Null

$asset = "dbmd-$version-windows-x86_64.exe"
$binary = Join-Path $release $asset
Copy-Item (Join-Path $root 'target\x86_64-pc-windows-msvc\release\dbmd.exe') $binary
$digest = (Get-FileHash -Algorithm SHA256 $binary).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path $trusted $asset), "$digest`n")

$probe = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
$probe.Start()
$port = ([Net.IPEndPoint]$probe.LocalEndpoint).Port
$probe.Stop()
$python = (Get-Command python).Source
$server = Start-Process -FilePath $python -ArgumentList @(
  '-m', 'http.server', "$port", '--bind', '127.0.0.1', '--directory', "`"$tmp`""
) -WindowStyle Hidden -PassThru

try {
  Start-Sleep -Milliseconds 500
  $env:PROCESSOR_ARCHITECTURE = 'AMD64'
  $env:DBMD_VERSION = $version
  $env:DBMD_BASE_URL = "http://127.0.0.1:$port/release"
  $env:DBMD_TRUSTED_MANIFEST_BASE = "http://127.0.0.1:$port/trusted"
  $env:DBMD_INSTALL_DIR = $installDir

  & (Join-Path $root 'scripts\install.ps1')
  $installed = Join-Path $installDir 'dbmd.exe'
  if (-not (Test-Path $installed)) { throw 'verified install did not write dbmd.exe' }
  if ((& $installed --version | Out-String) -notmatch [regex]::Escape($version)) {
    throw 'installed executable reported the wrong version'
  }

  # The destination itself cannot be a junction to an outside directory.
  $leafInstall = Join-Path $tmp 'leaf-link-install'
  $leafOutside = Join-Path $tmp 'leaf-link-outside'
  New-Item -ItemType Directory -Path $leafInstall | Out-Null
  New-Item -ItemType Directory -Path $leafOutside | Out-Null
  [IO.File]::WriteAllText((Join-Path $leafOutside 'dbmd.exe'), 'SAFE')
  New-Item -ItemType Junction -Path (Join-Path $leafInstall 'dbmd.exe') -Target $leafOutside | Out-Null
  $env:DBMD_INSTALL_DIR = $leafInstall
  $failed = $false
  try { & (Join-Path $root 'scripts\install.ps1') } catch {
    $failed = "$_" -match 'install destination must not be a reparse point'
  }
  if (-not $failed) { throw 'installer accepted a destination reparse point' }
  if ([IO.File]::ReadAllText((Join-Path $leafOutside 'dbmd.exe')) -ne 'SAFE') {
    throw 'destination reparse target was overwritten'
  }

  # Every parent directory component is opened without following reparse points.
  $parentOutside = Join-Path $tmp 'parent-link-outside'
  $parentInstall = Join-Path $tmp 'parent-link-install'
  New-Item -ItemType Directory -Path $parentOutside | Out-Null
  [IO.File]::WriteAllText((Join-Path $parentOutside 'dbmd.exe'), 'SAFE')
  New-Item -ItemType Junction -Path $parentInstall -Target $parentOutside | Out-Null
  $env:DBMD_INSTALL_DIR = $parentInstall
  $failed = $false
  try { & (Join-Path $root 'scripts\install.ps1') } catch {
    $failed = "$_" -match 'install directory must not contain reparse points'
  }
  if (-not $failed) { throw 'installer accepted a reparse-point install directory' }
  if ([IO.File]::ReadAllText((Join-Path $parentOutside 'dbmd.exe')) -ne 'SAFE') {
    throw 'install-directory reparse target was overwritten'
  }

  # A directory at the executable leaf is never treated as a move target.
  $directoryLeaf = Join-Path $tmp 'directory-leaf-install'
  New-Item -ItemType Directory -Path (Join-Path $directoryLeaf 'dbmd.exe') -Force | Out-Null
  $env:DBMD_INSTALL_DIR = $directoryLeaf
  $failed = $false
  try { & (Join-Path $root 'scripts\install.ps1') } catch {
    $failed = "$_" -match 'install destination must be absent or a regular file'
  }
  if (-not $failed) { throw 'installer accepted a directory destination' }
  if (Test-Path (Join-Path $directoryLeaf 'dbmd.exe\dbmd.exe')) {
    throw 'installer wrote into a directory destination'
  }

  # A malicious or stale trusted digest refuses before any filesystem write.
  [IO.File]::WriteAllText((Join-Path $trusted $asset), "$('0' * 64)`n")
  $env:DBMD_INSTALL_DIR = Join-Path $tmp 'bad-digest-install'
  $failed = $false
  try { & (Join-Path $root 'scripts\install.ps1') } catch {
    $failed = "$_" -match '(checksum mismatch|no trusted checksum)'
  }
  if (-not $failed) { throw 'installer accepted a bad independent digest' }
  if (Test-Path (Join-Path $env:DBMD_INSTALL_DIR 'dbmd.exe')) {
    throw 'installer wrote after digest refusal'
  }
  [IO.File]::WriteAllText((Join-Path $trusted $asset), "$digest`n")

  # A custom artifact mirror's colocated files never become the trust root.
  $env:DBMD_TRUSTED_MANIFEST_BASE = "http://127.0.0.1:$port/missing"
  $env:DBMD_INSTALL_DIR = Join-Path $tmp 'missing-manifest-install'
  $failed = $false
  try { & (Join-Path $root 'scripts\install.ps1') } catch {
    $failed = "$_" -match 'no trusted checksum'
  }
  if (-not $failed) { throw 'installer accepted a missing independent manifest' }
  if (Test-Path (Join-Path $env:DBMD_INSTALL_DIR 'dbmd.exe')) {
    throw 'installer wrote without an independent manifest'
  }

  $env:DBMD_VERSION = '../wrong'
  $failed = $false
  try { & (Join-Path $root 'scripts\install.ps1') } catch {
    $failed = "$_" -match 'version must be strict SemVer'
  }
  if (-not $failed) { throw 'installer accepted a path-like version' }
} finally {
  Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
  Remove-Item Env:DBMD_VERSION -ErrorAction SilentlyContinue
  Remove-Item Env:DBMD_BASE_URL -ErrorAction SilentlyContinue
  Remove-Item Env:DBMD_TRUSTED_MANIFEST_BASE -ErrorAction SilentlyContinue
  Remove-Item Env:DBMD_INSTALL_DIR -ErrorAction SilentlyContinue
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

exit 0
