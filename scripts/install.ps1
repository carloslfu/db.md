# db.md toolkit installer for Windows.
#
#   irm https://www.sevrahq.com/install/dbmd.ps1 | iex
#
# Downloads the native x64 executable and verifies its SHA-256 against Sevra's
# independently deployed release manifest before replacing the installed file.
# Windows on ARM uses the same executable through built-in x64 emulation.
#
# Honors DBMD_INSTALL_DIR, DBMD_VERSION, DBMD_BASE_URL, and
# DBMD_TRUSTED_MANIFEST_BASE. The final invocation is deliberately the LAST
# line, so a truncated `irm | iex` stream cannot execute a partial installer.

$ErrorActionPreference = 'Stop'

$Repo = 'carloslfu/db.md'
$Dir = if ($env:DBMD_INSTALL_DIR) { $env:DBMD_INSTALL_DIR } else { Join-Path $env:USERPROFILE '.dbmd\bin' }
$Base = if ($env:DBMD_BASE_URL) { $env:DBMD_BASE_URL.TrimEnd('/') } else { "https://github.com/$Repo/releases/download" }
$LatestUrl = 'https://www.sevrahq.com/api/hub/releases/dbmd/latest'
$ManifestBase = if ($env:DBMD_TRUSTED_MANIFEST_BASE) { $env:DBMD_TRUSTED_MANIFEST_BASE.TrimEnd('/') } else { 'https://www.sevrahq.com/api/hub/releases/dbmd' }

function Fail([string]$Message) { Write-Error "dbmd install: $Message" -ErrorAction Stop }
function Info([string]$Message) { Write-Host $Message }

# The final install is a filesystem security boundary. Path inspection and
# replacement use no-follow Windows handles, and every directory handle stays
# open without FILE_SHARE_DELETE until MoveFileExW completes.
if (-not ('DbmdInstallerNative' -as [type])) {
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

public static class DbmdInstallerNative {
  public const uint FILE_ATTRIBUTE_DIRECTORY = 0x10;
  public const uint FILE_ATTRIBUTE_REPARSE_POINT = 0x400;
  private const uint FILE_READ_ATTRIBUTES = 0x80;
  private const uint FILE_SHARE_READ = 0x1;
  private const uint FILE_SHARE_WRITE = 0x2;
  private const uint OPEN_EXISTING = 3;
  private const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
  private const uint FILE_FLAG_OPEN_REPARSE_POINT = 0x00200000;
  private const uint MOVEFILE_REPLACE_EXISTING = 0x1;
  private const uint MOVEFILE_WRITE_THROUGH = 0x8;

  [StructLayout(LayoutKind.Sequential)]
  public struct ByHandleFileInformation {
    public uint FileAttributes;
    public System.Runtime.InteropServices.ComTypes.FILETIME CreationTime;
    public System.Runtime.InteropServices.ComTypes.FILETIME LastAccessTime;
    public System.Runtime.InteropServices.ComTypes.FILETIME LastWriteTime;
    public uint VolumeSerialNumber;
    public uint FileSizeHigh;
    public uint FileSizeLow;
    public uint NumberOfLinks;
    public uint FileIndexHigh;
    public uint FileIndexLow;
  }

  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  private static extern SafeFileHandle CreateFileW(
    string fileName, uint desiredAccess, uint shareMode, IntPtr securityAttributes,
    uint creationDisposition, uint flagsAndAttributes, IntPtr templateFile);

  [DllImport("kernel32.dll", SetLastError = true)]
  private static extern bool GetFileInformationByHandle(
    SafeFileHandle file, out ByHandleFileInformation information);

  [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
  private static extern bool MoveFileExW(string existing, string replacement, uint flags);

  public static SafeFileHandle OpenNoFollow(string path) {
    return CreateFileW(
      path, FILE_READ_ATTRIBUTES, FILE_SHARE_READ | FILE_SHARE_WRITE,
      IntPtr.Zero, OPEN_EXISTING,
      FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, IntPtr.Zero);
  }

  public static uint Attributes(SafeFileHandle handle) {
    ByHandleFileInformation information;
    if (!GetFileInformationByHandle(handle, out information)) {
      throw new Win32Exception(Marshal.GetLastWin32Error());
    }
    return information.FileAttributes;
  }

  public static void Replace(string source, string destination) {
    if (!MoveFileExW(source, destination, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)) {
      throw new Win32Exception(Marshal.GetLastWin32Error());
    }
  }
}
'@
}

function Open-NoFollow([string]$Path, [bool]$AllowMissing = $false) {
  $handle = [DbmdInstallerNative]::OpenNoFollow($Path)
  if ($handle.IsInvalid) {
    $code = [Runtime.InteropServices.Marshal]::GetLastWin32Error()
    $handle.Dispose()
    if ($AllowMissing -and $code -in @(2, 3)) { return $null }
    throw [ComponentModel.Win32Exception]::new($code)
  }
  return $handle
}

function Open-LockedInstallDirectory([string]$Path) {
  $full = [IO.Path]::GetFullPath($Path)
  $root = [IO.Path]::GetPathRoot($full)
  if (-not $root) { Fail 'install directory must be an absolute filesystem path' }
  $handles = [Collections.Generic.List[Microsoft.Win32.SafeHandles.SafeFileHandle]]::new()
  $current = $root
  $parts = $full.Substring($root.Length) -split '[\/]'
  try {
    $components = @($root) + @($parts | Where-Object { $_ })
    foreach ($component in $components) {
      $handle = $null
      try {
        if ($component -ne $root) {
          $current = Join-Path $current $component
          $handle = Open-NoFollow $current $true
          if ($null -eq $handle) {
            [IO.Directory]::CreateDirectory($current) | Out-Null
            $handle = Open-NoFollow $current
          }
        } else {
          $handle = Open-NoFollow $current
        }
        $attributes = [DbmdInstallerNative]::Attributes($handle)
        if (($attributes -band [DbmdInstallerNative]::FILE_ATTRIBUTE_REPARSE_POINT) -ne 0) {
          Fail "install directory must not contain reparse points: $current"
        }
        if (($attributes -band [DbmdInstallerNative]::FILE_ATTRIBUTE_DIRECTORY) -eq 0) {
          Fail "install path component is not a directory: $current"
        }
        $handles.Add($handle)
        $handle = $null
      } finally {
        if ($handle) { $handle.Dispose() }
      }
    }
    return [pscustomobject]@{ FullPath = $full; Handles = $handles }
  } catch {
    foreach ($handle in $handles) { $handle.Dispose() }
    throw
  }
}

function Assert-SafeInstallLeaf([string]$Path) {
  $handle = Open-NoFollow $Path $true
  if ($null -eq $handle) { return }
  try {
    $attributes = [DbmdInstallerNative]::Attributes($handle)
    if (($attributes -band [DbmdInstallerNative]::FILE_ATTRIBUTE_REPARSE_POINT) -ne 0) {
      Fail "install destination must not be a reparse point: $Path"
    }
    if (($attributes -band [DbmdInstallerNative]::FILE_ATTRIBUTE_DIRECTORY) -ne 0) {
      Fail "install destination must be absent or a regular file: $Path"
    }
  } finally {
    $handle.Dispose()
  }
}

function Fetch([string]$Url, [string]$OutFile) {
  try {
    Invoke-WebRequest -Uri $Url -OutFile $OutFile -UseBasicParsing
  } catch {
    Fail ("download failed: $Url`n  This release may predate Windows support; " +
      'pin a newer DBMD_VERSION, or install under WSL with the sh installer.')
  }
}

function Invoke-Main {
  $arch = $env:PROCESSOR_ARCHITECTURE
  switch -Regex ($arch) {
    '^(AMD64|ARM64)$' { }
    default { Fail "unsupported arch: $arch (x64 and ARM64-via-emulation only)" }
  }
  if ($arch -eq 'ARM64') {
    Info 'note: ARM64 detected; installing the x64 binary through Windows emulation.'
  }

  $version = $env:DBMD_VERSION
  if (-not $version) {
    Info 'Resolving the latest dbmd release...'
    try { $version = "$(Invoke-RestMethod -Uri $LatestUrl -UseBasicParsing)".Trim() } catch {
      Fail 'could not resolve the trusted latest release; pin DBMD_VERSION to retry'
    }
  }
  $version = "$version".Trim()
  if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$') {
    Fail 'version must be strict SemVer without a leading v'
  }

  $assetName = "dbmd-$version-windows-x86_64.exe"
  $url = "$Base/v$version/$assetName"
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("dbmd-install-" + [IO.Path]::GetRandomFileName())
  New-Item -ItemType Directory -Path $tmp | Out-Null
  $stageDir = $null
  $lockedDir = $null
  try {
    Info "Downloading dbmd $version (windows-x86_64)..."
    $bin = Join-Path $tmp 'dbmd.exe'
    Fetch $url $bin

    # A custom artifact mirror never silently becomes its own trust root.
    # Fetch a fresh digest from the separately configured trusted origin.
    $nonce = [Guid]::NewGuid().ToString('N')
    $manifestResource = "$ManifestBase/$version/$assetName"
    $separator = if ($manifestResource -match '\?') { '&' } else { '?' }
    try {
      $expected = "$(Invoke-RestMethod -Uri "${manifestResource}${separator}nonce=$nonce" -UseBasicParsing -Headers @{
        'Cache-Control' = 'no-cache, no-store'
        'Pragma' = 'no-cache'
      })".Trim().ToLowerInvariant()
    } catch {
      Fail "no trusted checksum for dbmd $version $assetName"
    }
    if ($expected -notmatch '^[0-9a-f]{64}$') {
      Fail "no trusted checksum for dbmd $version $assetName"
    }
    $actual = (Get-FileHash -Algorithm SHA256 -Path $bin).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
      Fail "checksum mismatch (expected $expected, got $actual). Refusing to install"
    }
    Info 'checksum: verified (sha256)'

    $reported = (& $bin --version | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or $reported -notmatch "^dbmd $([regex]::Escape($version))(\s|$)") {
      Fail "verified executable reported an unexpected version: $reported"
    }

    $lockedDir = Open-LockedInstallDirectory $Dir
    $Dir = $lockedDir.FullPath
    $stageDir = Join-Path $Dir (".dbmd-stage-" + [IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $stageDir | Out-Null
    $stageHandle = Open-NoFollow $stageDir
    $stageAttributes = [DbmdInstallerNative]::Attributes($stageHandle)
    if (($stageAttributes -band [DbmdInstallerNative]::FILE_ATTRIBUTE_REPARSE_POINT) -ne 0 -or
        ($stageAttributes -band [DbmdInstallerNative]::FILE_ATTRIBUTE_DIRECTORY) -eq 0) {
      $stageHandle.Dispose()
      Fail 'private installer stage became a reparse point or non-directory'
    }
    $lockedDir.Handles.Add($stageHandle)
    $staged = Join-Path $stageDir 'dbmd.exe'
    $sourceStream = [IO.File]::Open($bin, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
      $stageStream = [IO.File]::Open($staged, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::None)
      try {
        $sourceStream.CopyTo($stageStream)
        $stageStream.Flush($true)
      } finally {
        $stageStream.Dispose()
      }
    } finally {
      $sourceStream.Dispose()
    }

    $destination = Join-Path $Dir 'dbmd.exe'
    Assert-SafeInstallLeaf $destination
    [DbmdInstallerNative]::Replace($staged, $destination)
    foreach ($handle in $lockedDir.Handles) { $handle.Dispose() }
    $lockedDir = $null
    Remove-Item -Path $stageDir -Force
    $stageDir = $null
    Info "dbmd $version installed to $destination"

    if (($env:Path -split ';') -contains $Dir) {
      Info 'Run: dbmd --help'
    } else {
      Info 'Add it to your PATH (user scope, then open a new shell):'
      Info "  [Environment]::SetEnvironmentVariable('Path', `"$Dir;`" + [Environment]::GetEnvironmentVariable('Path','User'), 'User')"
      Info "  `$env:Path = `"$Dir;`" + `$env:Path"
    }
  } finally {
    if ($lockedDir) {
      foreach ($handle in $lockedDir.Handles) { $handle.Dispose() }
    }
    if ($stageDir) {
      Remove-Item -Recurse -Force $stageDir -ErrorAction SilentlyContinue
    }
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }
}

Invoke-Main
