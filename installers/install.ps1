<#
.SYNOPSIS
    MemFork installer for Windows.

.DESCRIPTION
    irm https://github.com/memforkdb/memfork/releases/latest/download/install.ps1 | iex

    Installs one executable into a directory you own. No administrator, no
    package manager, nothing outside your profile. Set MEMFORK_VERSION to pin
    a release and MEMFORK_INSTALL_DIR to choose where it goes.

    Why this is not the installer `dist` generates: Windows will not let a
    running executable be replaced, and an older MemFork may have a daemon
    serving your memory right now. That daemon has to be stopped first, and a
    generated installer cannot run that step.

    Works on Windows PowerShell 5.1 and PowerShell 7, on x64 and arm64.
#>

# 5.1 has no `$PSNativeCommandUseErrorActionPreference`, so errors are checked
# by hand rather than relied upon.
Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$Repo = 'memforkdb/memfork'
$Version = if ($env:MEMFORK_VERSION) { $env:MEMFORK_VERSION } else { 'latest' }
$InstallDir = if ($env:MEMFORK_INSTALL_DIR) {
    $env:MEMFORK_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA 'Programs\memfork\bin'
}

function Write-Step([string]$Message) { Write-Host $Message }

function Stop-WithMessage([string]$Message) {
    Write-Host "memfork: $Message" -ForegroundColor Red
    exit 1
}

function Get-Target {
    # An explicit choice wins, the same as on the other platforms.
    if ($env:MEMFORK_TARGET) { return $env:MEMFORK_TARGET }
    # PROCESSOR_ARCHITECTURE is the architecture of *this process*; on arm64
    # an x64 PowerShell reports AMD64, and installing the x64 build there is
    # correct — it is what the shell can run.
    $arch = $env:PROCESSOR_ARCHITECTURE
    if (-not $arch) { $arch = 'AMD64' }
    switch ($arch.ToUpperInvariant()) {
        'AMD64' { return 'x86_64-pc-windows-msvc' }
        'ARM64' { return 'aarch64-pc-windows-msvc' }
        'X86'   {
            # A 32-bit shell on a 64-bit machine: install the build the
            # machine can run, not the one this shell happens to be.
            if ($env:PROCESSOR_ARCHITEW6432 -eq 'ARM64') { return 'aarch64-pc-windows-msvc' }
            return 'x86_64-pc-windows-msvc'
        }
        default { Stop-WithMessage "unsupported architecture $arch" }
    }
}

function Get-DownloadUrl([string]$Name) {
    # MEMFORK_DOWNLOAD_BASE points somewhere other than GitHub: a mirror, a
    # cache inside a network that cannot reach it, or the stand-in release the
    # installer tests serve from disk. Without it these tests would have to
    # rewrite the script, and then they would not be testing this script.
    if ($env:MEMFORK_DOWNLOAD_BASE) {
        return "$($env:MEMFORK_DOWNLOAD_BASE.TrimEnd('/'))/$Name"
    }
    if ($Version -eq 'latest') {
        return "https://github.com/$Repo/releases/latest/download/$Name"
    }
    return "https://github.com/$Repo/releases/download/$Version/$Name"
}

function Save-File([string]$Url, [string]$Path) {
    try {
        # 5.1 defaults to TLS 1.0 and shows a progress bar that makes downloads
        # crawl. Both are fixed here rather than left to surprise somebody.
        [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
        $previous = $ProgressPreference
        $ProgressPreference = 'SilentlyContinue'
        try {
            Invoke-WebRequest -Uri $Url -OutFile $Path -UseBasicParsing
        } finally {
            $ProgressPreference = $previous
        }
    } catch {
        Stop-WithMessage "could not download $Url ($($_.Exception.Message))"
    }
}

# The release publishes one `<archive>.sha256` beside each archive, holding
# `<hash> *<filename>`. Anything that is not an exact match stops the install.
function Test-Checksum([string]$Archive, [string]$SumFile) {
    $name = Split-Path $Archive -Leaf
    $expected = ((Get-Content $SumFile -Raw) -split '\s+')[0]
    if (-not ($expected -match '^[0-9a-fA-F]{64}$')) {
        Stop-WithMessage "no usable checksum published for $name; refusing to install"
    }
    $actual = (Get-FileHash -Path $Archive -Algorithm SHA256).Hash
    if ($actual.ToLowerInvariant() -ne $expected.ToLowerInvariant()) {
        Stop-WithMessage "checksum mismatch for $name; refusing to install"
    }
    Write-Host "Checksum verified (sha256 $($expected.ToLowerInvariant()))."
}

function Stop-ExistingMemfork([string]$Exe) {
    if (-not (Test-Path -LiteralPath $Exe)) { return }
    Write-Step 'Stopping the MemFork already installed here, so its daemon is not'
    Write-Step 'left serving your memory from an old build.'
    try {
        & $Exe stop *> $null
    } catch {
        # Nothing running is the usual case, and not a problem.
    }
    # Windows holds an executable open while it runs, so the replace below
    # fails unless the daemon has really gone. Give it a moment to exit.
    for ($i = 0; $i -lt 50; $i++) {
        try {
            $handle = [IO.File]::Open($Exe, 'Open', 'Write', 'None')
            $handle.Close()
            return
        } catch {
            Start-Sleep -Milliseconds 100
        }
    }
    Stop-WithMessage "$Exe is still running; close it and try again"
}

function Add-ToUserPath([string]$Directory) {
    # The user's own PATH, in the registry. Never the machine's, which needs
    # administrator, and never `setx`, which truncates at 1024 characters.
    $current = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $current) { $current = '' }
    $parts = $current -split ';' | Where-Object { $_ -ne '' }
    $added = -not ($parts -contains $Directory)
    if ($added) {
        $updated = (@($parts) + $Directory) -join ';'
        [Environment]::SetEnvironmentVariable('Path', $updated, 'User')
    }

    # The registry is for the next terminal. This one is running the installer
    # right now — through `irm ... | iex`, in the session the person is sitting
    # in — and it would be strange to install a command they cannot then type.
    # `$env:Path` is this process's copy, and children inherit it, so setting
    # it here makes `memfork` work immediately.
    $live = $env:Path -split ';' | Where-Object { $_ -ne '' }
    if (-not ($live -contains $Directory)) {
        $env:Path = ($live + $Directory) -join ';'
    }
    return $added
}

$target = Get-Target
$archive = "memfork-$target.zip"
$temp = Join-Path ([IO.Path]::GetTempPath()) ("memfork-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $temp | Out-Null

try {
    Write-Step "Downloading MemFork for $target..."
    $archivePath = Join-Path $temp $archive
    $sumPath = "$archivePath.sha256"
    Save-File (Get-DownloadUrl $archive) $archivePath
    Save-File (Get-DownloadUrl "$archive.sha256") $sumPath
    Test-Checksum $archivePath $sumPath

    Expand-Archive -Path $archivePath -DestinationPath $temp -Force
    $binary = Get-ChildItem -Path $temp -Filter 'memfork.exe' -Recurse |
        Select-Object -First 1
    if (-not $binary) { Stop-WithMessage 'the archive did not contain memfork.exe' }

    $destination = Join-Path $InstallDir 'memfork.exe'
    Stop-ExistingMemfork $destination

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    Copy-Item -LiteralPath $binary.FullName -Destination $destination -Force

    $version = (& $destination --version) 2>$null
    if (-not $version) { $version = 'memfork' }

    Write-Host ''
    Write-Host "Installed $version"
    Write-Host "  $destination"
    Write-Host ''

    if (Add-ToUserPath $InstallDir) {
        Write-Host "Added $InstallDir to your PATH, for this terminal and the next."
        Write-Host ''
    }

    Write-Host 'Next:'
    Write-Host '  memfork init      register MemFork with the MCP clients you have'
    Write-Host '  memfork doctor    check what is installed and what is talking to it'
    Write-Host ''
    Write-Host 'To uninstall:'
    Write-Host '  memfork stop'
    Write-Host "  Remove-Item -Recurse -Force '$InstallDir'"
    Write-Host '  [Environment]::SetEnvironmentVariable(''Path'', (([Environment]::GetEnvironmentVariable(''Path'',''User'') -split '';'' | Where-Object { $_ -ne ''' -NoNewline
    Write-Host "$InstallDir" -NoNewline
    Write-Host ''' }) -join '';''), ''User'')'
    Write-Host ''
    Write-Host 'That leaves your stored memory, which lives in the data directory'
    Write-Host '`memfork doctor` prints. Delete that too if you want it gone.'
} finally {
    Remove-Item -Recurse -Force -LiteralPath $temp -ErrorAction SilentlyContinue
}
