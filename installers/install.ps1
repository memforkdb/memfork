<#
.SYNOPSIS
    MemFork installer for Windows.

.DESCRIPTION
    irm https://github.com/memforkdb/memfork/releases/latest/download/install.ps1 | iex

    Installs one executable into a directory you own. No administrator, no
    package manager, nothing outside your profile. MEMFORK_VERSION pins a
    release, MEMFORK_INSTALL_DIR chooses where it goes, MEMFORK_TARGET picks a
    build other than this machine's, and MEMFORK_DOWNLOAD_BASE points somewhere
    other than GitHub. MEMFORK_GITHUB_BASE stands in for https://github.com
    itself, for a GitHub Enterprise mirror or the installer tests.

    Why this is not the installer `dist` generates: Windows will not let a
    running executable be replaced, and an older MemFork may have a daemon
    serving your memory right now. That daemon has to be stopped first, and a
    generated installer cannot run that step.

    Works on Windows PowerShell 5.1 and PowerShell 7, on x64 and arm64.
#>

# This script runs *inside the caller's PowerShell session*: `irm ... | iex`
# evaluates it as though it had been typed at their prompt. Two rules follow.
#
# It never calls `exit`. In an iex'd script `exit` ends the host, which closes
# the person's terminal — the window they were about to type `memfork init`
# into. Every failure is a message and a return instead.
#
# It leaves nothing behind. Everything runs inside this one script block, so
# its strict mode, its error preference, its functions and its variables all
# belong to the block and vanish with it. The only change meant to outlive the
# install is `$env:Path`, which is process-wide by nature.
& {
    Set-StrictMode -Version 2.0
    $ErrorActionPreference = 'Stop'

    $Repo = 'memforkdb/memfork'
    $GitHub = if ($env:MEMFORK_GITHUB_BASE) { $env:MEMFORK_GITHUB_BASE.TrimEnd('/') } else { 'https://github.com' }
    $Version = if ($env:MEMFORK_VERSION) { $env:MEMFORK_VERSION } else { 'latest' }
    $InstallDir = if ($env:MEMFORK_INSTALL_DIR) {
        $env:MEMFORK_INSTALL_DIR
    } else {
        Join-Path $env:LOCALAPPDATA 'Programs\memfork\bin'
    }

    # A failure this installer means to report, as opposed to one it did not
    # see coming. The prefix is how the handler at the bottom tells them apart.
    $FailurePrefix = 'memfork-install: '

    function Write-Step([string]$Message) { Write-Host $Message }

    # Stop the install. Throws rather than exits — see the note at the top.
    function Stop-WithMessage([string]$Message) {
        throw ($FailurePrefix + $Message)
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

    # Turn "latest" into the one version it means right now, and use that
    # exact version for every download that follows.
    #
    # GitHub's `releases/latest/download/<file>` is a redirect answered per
    # request, and a release being published, or a stale edge cache, can
    # answer two requests with two versions: an archive from one release and
    # a checksum from another, or an older build than the release page shows.
    # That happened on an earlier release. So the version is resolved once, from the
    # redirect `releases/latest` sends, and never again.
    function Resolve-LatestVersion {
        $url = "$GitHub/$Repo/releases/latest"
        try {
            [Net.ServicePointManager]::SecurityProtocol =
                [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
            $request = [System.Net.HttpWebRequest]::Create($url)
            $request.Method = 'HEAD'
            $request.AllowAutoRedirect = $false
            $response = $request.GetResponse()
            try {
                $location = [string]$response.Headers['Location']
            } finally {
                $response.Dispose()
            }
        } catch {
            Stop-WithMessage "could not ask $GitHub which release is the latest ($($_.Exception.Message))"
        }
        $tag = $location -replace '^.*/tag/', ''
        if (-not ($tag -match '^v[0-9]')) {
            Stop-WithMessage "could not work out the latest release from '$location'; set MEMFORK_VERSION to a release tag of the form vX.Y.Z"
        }
        return $tag
    }

    function Get-DownloadUrl([string]$Name) {
        # MEMFORK_DOWNLOAD_BASE points somewhere other than GitHub: a mirror, a
        # cache inside a network that cannot reach it, or the stand-in release
        # the installer tests serve from disk.
        if ($env:MEMFORK_DOWNLOAD_BASE) {
            return "$($env:MEMFORK_DOWNLOAD_BASE.TrimEnd('/'))/$Name"
        }
        return "$GitHub/$Repo/releases/download/$Version/$Name"
    }

    # A progress bar is for a person watching a console: never into a pipe or
    # a log, never in CI, and not when NO_COLOR asks for plain output.
    # MEMFORK_PROGRESS=1 forces it, which is how the installer tests reach it.
    function Test-ShowProgress {
        if ($env:MEMFORK_PROGRESS -eq '1') { return $true }
        if ($env:CI -or $env:NO_COLOR) { return $false }
        if (-not [Environment]::UserInteractive) { return $false }
        if ([Console]::IsOutputRedirected) { return $false }
        return $Host.Name -eq 'ConsoleHost'
    }

    function Save-File([string]$Url, [string]$Path, [string]$Showing) {
        try {
            # 5.1 may not offer TLS 1.2 by default. Added to what the session
            # already allows rather than replacing it, because that setting is
            # process-wide and outlives this script.
            [Net.ServicePointManager]::SecurityProtocol =
                [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
            if ($Showing -and (Test-ShowProgress)) {
                # Streamed by hand rather than through Invoke-WebRequest,
                # whose own progress bar makes 5.1 downloads crawl: this one
                # redraws once per whole percent.
                $response = [System.Net.HttpWebRequest]::Create($Url).GetResponse()
                $total = $response.ContentLength
                $in = $response.GetResponseStream()
                $out = [System.IO.File]::Create($Path)
                try {
                    $buffer = New-Object byte[] 65536
                    $done = 0
                    $shown = -1
                    while (($read = $in.Read($buffer, 0, $buffer.Length)) -gt 0) {
                        $out.Write($buffer, 0, $read)
                        $done += $read
                        if ($total -gt 0) {
                            $percent = [int][Math]::Floor(100 * $done / $total)
                            if ($percent -ne $shown) {
                                $size = '{0:N1} MB' -f ($total / 1MB)
                                Write-Progress -Activity $Showing -Status "$percent% of $size" -PercentComplete $percent
                                $shown = $percent
                            }
                        }
                    }
                } finally {
                    $out.Dispose()
                    $in.Dispose()
                    $response.Dispose()
                    Write-Progress -Activity $Showing -Completed
                }
            } else {
                # 5.1's own progress bar makes downloads crawl. This
                # preference is scoped to the block, so it needs no restoring.
                $ProgressPreference = 'SilentlyContinue'
                Invoke-WebRequest -Uri $Url -OutFile $Path -UseBasicParsing
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

    # Clients this installer can name, by executable. Anything not listed is
    # described by its own file description, so an unknown client is still
    # named rather than left as "something".
    $KnownClients = @{
        'claude.exe'   = 'Claude Code'
        'codex.exe'    = 'Codex'
        'cursor.exe'   = 'Cursor'
        'code.exe'     = 'Visual Studio Code'
        'windsurf.exe' = 'Windsurf'
        'zed.exe'      = 'Zed'
        'gemini.exe'   = 'Gemini CLI'
        'grok.exe'     = 'Grok'
        'chatgpt.exe'  = 'ChatGPT'
    }

    # Processes that run a command on somebody else's behalf. A client that
    # starts `memfork mcp` through a shell is still the client, so these are
    # walked past on the way up to whoever asked.
    $Intermediaries = @('cmd.exe', 'conhost.exe', 'powershell.exe', 'pwsh.exe',
                        'bash.exe', 'sh.exe', 'openconsole.exe')

    # The application a process belongs to: the nearest ancestor that is not a
    # shell or a console host.
    function Get-OwningApplication($Process) {
        $current = $Process
        for ($depth = 0; $depth -lt 8; $depth++) {
            $parentId = $current.ParentProcessId
            if (-not $parentId) { return $null }
            $parent = Get-CimInstance Win32_Process -Filter "ProcessId = $parentId" -ErrorAction SilentlyContinue
            if (-not $parent) { return $null }
            $exe = ([string]$parent.Name).ToLowerInvariant()
            if ($Intermediaries -notcontains $exe) { return $parent }
            $current = $parent
        }
        return $null
    }

    function Get-FriendlyName($Process) {
        $exe = ([string]$Process.Name).ToLowerInvariant()
        if ($KnownClients.ContainsKey($exe)) { return $KnownClients[$exe] }
        $description = $null
        try {
            $description = (Get-Process -Id $Process.ProcessId -ErrorAction Stop).Description
        } catch { }
        if ($description) { return $description }
        return [IO.Path]::GetFileNameWithoutExtension([string]$Process.Name)
    }

    # Say who is holding the executable, and what to do about each of them.
    # Nothing here stops a client: closing somebody's editor, or their agent
    # halfway through a task, is not an installer's decision to make.
    function Get-HolderAdvice([string]$Exe) {
        $full = [IO.Path]::GetFullPath($Exe)
        $running = @(Get-CimInstance Win32_Process -Filter "Name = 'memfork.exe'" -ErrorAction SilentlyContinue |
            Where-Object {
                $_.ExecutablePath -and ([IO.Path]::GetFullPath([string]$_.ExecutablePath) -eq $full)
            })

        $lines = @()
        $seen = @{}
        foreach ($proc in $running) {
            $commandLine = [string]$proc.CommandLine
            if ($commandLine -match '\sserve(\s|$)') {
                # `memfork stop` was already asked; a daemon still here did not
                # listen, and that is the one thing that can be retried as-is.
                $lines += "The MemFork daemon (pid $($proc.ProcessId)) did not stop. Run ``memfork stop``, then run this installer again."
                continue
            }
            $owner = Get-OwningApplication $proc
            if ($owner) {
                if ($seen.ContainsKey($owner.ProcessId)) { continue }
                $seen[$owner.ProcessId] = $true
                $name = Get-FriendlyName $owner
                $lines += "$name (pid $($owner.ProcessId)) is using MemFork. Close it, then run this installer again."
            } else {
                $lines += "A MemFork process (pid $($proc.ProcessId)) is still running with nothing that started it. End it in Task Manager, then run this installer again."
            }
        }

        if ($lines.Count -eq 0) {
            # Locked, but by nothing we can see — another account's process,
            # or a scanner holding the file for a moment.
            $lines += "$Exe is in use by another program. Close anything that might be running MemFork, then run this installer again."
        }
        return ($lines -join [Environment]::NewLine)
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
        # fails while anything is still running it. Give the daemon a moment to
        # exit, then see what is left.
        for ($i = 0; $i -lt 50; $i++) {
            try {
                $handle = [IO.File]::Open($Exe, 'Open', 'Write', 'None')
                $handle.Close()
                return
            } catch {
                Start-Sleep -Milliseconds 100
            }
        }
        Stop-WithMessage (Get-HolderAdvice $Exe)
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

        # The registry is for the next terminal. This one is running the
        # installer right now, and it would be strange to install a command the
        # person cannot then type. `$env:Path` is this process's copy — the one
        # change this script means to leave behind.
        $live = $env:Path -split ';' | Where-Object { $_ -ne '' }
        if (-not ($live -contains $Directory)) {
            $env:Path = (@($live) + $Directory) -join ';'
        }
        return $added
    }

    $temp = $null
    try {
        $target = Get-Target
        $archive = "memfork-$target.zip"
        if (-not $env:MEMFORK_DOWNLOAD_BASE -and $Version -eq 'latest') {
            $Version = Resolve-LatestVersion
            Write-Step "Latest release is $Version."
        }
        $temp = Join-Path ([IO.Path]::GetTempPath()) ("memfork-" + [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $temp | Out-Null

        Write-Step "Downloading MemFork for $target..."
        $archivePath = Join-Path $temp $archive
        $sumPath = "$archivePath.sha256"
        Save-File (Get-DownloadUrl $archive) $archivePath "Downloading MemFork for $target"
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

        $installed = (& $destination --version) 2>$null
        if (-not $installed) { $installed = 'memfork' }

        Write-Host ''
        Write-Host "Installed $installed"
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
        Write-Host 'Shell completions: memfork completions powershell | Out-String | Invoke-Expression'
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
    } catch {
        $message = [string]$_.Exception.Message
        if ($message.StartsWith($FailurePrefix)) {
            $message = $message.Substring($FailurePrefix.Length)
        } else {
            $message = "unexpected error: $message"
        }
        Write-Host ''
        foreach ($line in ($message -split "`r?`n")) {
            Write-Host "memfork: $line" -ForegroundColor Red
        }
        # One fixed line, so a script driving this installer — the tests do —
        # can tell failure from success without an exit code to read.
        Write-Host 'MemFork was not installed.' -ForegroundColor Red
    } finally {
        if ($temp) {
            Remove-Item -Recurse -Force -LiteralPath $temp -ErrorAction SilentlyContinue
        }
    }
}
