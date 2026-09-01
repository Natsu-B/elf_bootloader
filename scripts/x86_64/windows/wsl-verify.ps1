param(
    [string]$Message = '',
    [ValidateRange(0, 10080)]
    [int]$DailySoakMinutes = 0,
    [ValidateRange(0, 10000)]
    [int]$DailySoakRounds = 0,
    [string]$DailySoakExternalUrl = '',
    [string]$DailySoakRunId = ''
)

$ErrorActionPreference = 'Stop'
$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$statusFile = Join-Path $stateDirectory 'wsl-status.txt'
$phaseFile = Join-Path $stateDirectory 'daily-soak-phase.json'
New-Item -ItemType Directory -Force -Path $stateDirectory | Out-Null

function Write-Com2 {
    param([string[]]$Lines)

    $lastError = $null
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        $serial = $null
        try {
            $serial = [System.IO.Ports.SerialPort]::new(
                'COM2',
                115200,
                [System.IO.Ports.Parity]::None,
                8,
                [System.IO.Ports.StopBits]::One
            )
            $serial.Open()
            foreach ($line in $Lines) {
                $serial.WriteLine($line)
            }
            return
        } catch {
            $lastError = $_
        } finally {
            if ($null -ne $serial) {
                if ($serial.IsOpen) {
                    $serial.Close()
                }
                $serial.Dispose()
            }
        }
        Start-Sleep -Seconds 1
    }
    throw $lastError
}

function Assert-Soak {
    param([bool]$Condition, [string]$Failure)

    if (-not $Condition) {
        throw $Failure
    }
}

function ConvertTo-Hex {
    param([byte[]]$Bytes)

    return ([System.BitConverter]::ToString($Bytes) -replace '-', '').ToLowerInvariant()
}

function Get-ExternalUri {
    param([string]$Text)

    if (-not $Text) {
        return $null
    }
    $uri = [Uri]::new($Text, [UriKind]::Absolute)
    Assert-Soak ($uri.Scheme -eq 'https') 'daily soak external URL must use HTTPS'
    Assert-Soak (-not $uri.UserInfo -and -not $uri.Query -and -not $uri.Fragment) `
        'daily soak external URL must not contain credentials, a query, or a fragment'
    return $uri
}

function Invoke-ExternalProbe {
    param([Uri]$Uri)

    if ($null -eq $Uri) {
        return ''
    }

    Add-Type -AssemblyName System.Net.Http
    $client = [System.Net.Http.HttpClient]::new()
    $client.Timeout = [TimeSpan]::FromSeconds(30)
    $client.MaxResponseContentBufferSize = 16MB
    try {
        $payload = $client.GetByteArrayAsync($Uri).GetAwaiter().GetResult()
    } finally {
        $client.Dispose()
    }
    Assert-Soak ($payload.Length -gt 0 -and $payload.Length -le 16MB) `
        'Windows external download was empty or exceeded 16 MiB'
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $windowsHash = ConvertTo-Hex ($sha.ComputeHash($payload))
    $sha.Dispose()

    $externalPath = '/tmp/thin-hv-external.bin'
    try {
        $fetch = @(& wsl.exe --distribution ThinHvTest --user root --cd / --exec `
                /bin/busybox timeout -s KILL 45 /bin/busybox wget `
                -q -T 30 --no-check-certificate `
                -O $externalPath $Uri.AbsoluteUri 2>&1)
        Assert-Soak ($LASTEXITCODE -eq 0) `
            "WSL external download failed: $($fetch -join ' ')"
        $hashOutput = @(& wsl.exe --distribution ThinHvTest --user root --cd / --exec `
                /bin/busybox sha256sum $externalPath 2>&1)
        $hashText = (($hashOutput -join "`n") -replace "`0", '')
        Assert-Soak ($LASTEXITCODE -eq 0 -and $hashText -match '(?m)^([0-9a-f]{64})\s+') `
            "WSL external hash failed: $hashText"
        $wslHash = $Matches[1]
    } finally {
        & wsl.exe --distribution ThinHvTest --user root --cd / --exec `
            /bin/busybox rm -f $externalPath 2>&1 | Out-Null
    }
    # The minimal WSL rootfs has no CA bundle. Windows validates TLS, then this
    # byte-for-byte comparison protects the opt-in BusyBox transfer.
    Assert-Soak ($wslHash -eq $windowsHash) 'Windows and WSL external download hashes differ'
    return $windowsHash
}

function Invoke-DailyWorkload {
    param([string]$Phase, [Uri]$ExternalUri)

    $timer = [System.Diagnostics.Stopwatch]::StartNew()
    $feature = Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V
    $present = [bool](Get-CimInstance Win32_ComputerSystem).HypervisorPresent
    $vmms = Get-Service -Name vmms -ErrorAction SilentlyContinue
    Assert-Soak ($feature.State -eq 'Enabled') 'Hyper-V feature is not enabled'
    Assert-Soak $present 'Windows does not report a running hypervisor'
    Assert-Soak ($null -ne $vmms -and $vmms.Status -eq 'Running') 'vmms is not running'

    $memory = [byte[]]::new(256MB)
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    $rng.GetBytes($memory)
    $rng.Dispose()
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $memoryHash = ConvertTo-Hex ($sha.ComputeHash($memory))
    $sha.Dispose()
    Assert-Soak ($memoryHash -match '^[0-9a-f]{64}$') 'memory hash failed'
    $memory = $null
    [GC]::Collect()

    $block = [byte[]]::new(1MB)
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    $rng.GetBytes($block)
    $rng.Dispose()
    $diskPath = Join-Path $stateDirectory "daily-soak-$Phase.bin"
    $stream = [System.IO.File]::Open(
        $diskPath,
        [System.IO.FileMode]::Create,
        [System.IO.FileAccess]::Write,
        [System.IO.FileShare]::None
    )
    try {
        for ($index = 0; $index -lt 128; $index++) {
            $stream.Write($block, 0, $block.Length)
        }
        $stream.Flush($true)
    } finally {
        $stream.Dispose()
    }
    $diskHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $diskPath).Hash.ToLowerInvariant()
    $diskHashAgain = (Get-FileHash -Algorithm SHA256 -LiteralPath $diskPath).Hash.ToLowerInvariant()
    Assert-Soak ($diskHash -eq $diskHashAgain) 'disk write/read hash mismatch'
    Assert-Soak ((Get-Item -LiteralPath $diskPath).Length -eq 128MB) 'disk file length mismatch'

    $listener = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $accept = $listener.BeginAcceptTcpClient($null, $null)
    $client = [System.Net.Sockets.TcpClient]::new()
    $client.Connect($listener.LocalEndpoint.Address, $listener.LocalEndpoint.Port)
    $server = $listener.EndAcceptTcpClient($accept)
    $received = [byte[]]::new($block.Length)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $blockHash = ConvertTo-Hex ($sha.ComputeHash($block))
    $sha.Dispose()
    try {
        $sender = $client.GetStream()
        $receiver = $server.GetStream()
        for ($round = 0; $round -lt 64; $round++) {
            $pending = $sender.BeginWrite($block, 0, $block.Length, $null, $null)
            $offset = 0
            while ($offset -lt $received.Length) {
                $count = $receiver.Read($received, $offset, $received.Length - $offset)
                Assert-Soak ($count -gt 0) 'loopback connection closed early'
                $offset += $count
            }
            $sender.EndWrite($pending)
            $sha = [System.Security.Cryptography.SHA256]::Create()
            $receivedHash = ConvertTo-Hex ($sha.ComputeHash($received))
            $sha.Dispose()
            Assert-Soak ($receivedHash -eq $blockHash) 'loopback payload hash mismatch'
        }
    } finally {
        $client.Dispose()
        $server.Dispose()
        $listener.Stop()
    }

    $list = @(& wsl.exe --list --verbose 2>&1)
    $listText = (($list -join "`n") -replace "`0", '')
    Assert-Soak ($LASTEXITCODE -eq 0 -and `
            $listText -match '(?m)^\s*\*?\s*ThinHvTest\s+\S+\s+2\s*$') `
        'ThinHvTest is not a WSL2 distribution'
    $cpuHash = @(& wsl.exe --distribution ThinHvTest --user root --cd / --exec `
            /bin/busybox sha256sum /proc/cpuinfo 2>&1)
    Assert-Soak ($LASTEXITCODE -eq 0 -and (($cpuHash -join "`n") -match '^[0-9a-f]{64}')) `
        'WSL cpuinfo hash failed'
    $wslMemoryHash = @(& wsl.exe --distribution ThinHvTest --user root --cd / --exec `
            /bin/sh -c '/bin/busybox dd if=/dev/zero bs=1M count=64 2>/dev/null | /bin/busybox sha256sum' `
            2>&1)
    Assert-Soak ($LASTEXITCODE -eq 0 -and (($wslMemoryHash -join "`n") -match `
            '^3b6a07d0d404fab4e23b6d34bc6696a6a312dd92821332385e5af7c01c421351\s')) `
        'WSL memory hash failed'
    $externalHash = Invoke-ExternalProbe -Uri $ExternalUri
    & wsl.exe --shutdown | Out-Null
    Assert-Soak ($LASTEXITCODE -eq 0) 'wsl --shutdown failed'

    $timer.Stop()
    return [pscustomobject]@{
        elapsed_ms = $timer.ElapsedMilliseconds
        memory_sha256 = $memoryHash
        disk_sha256 = $diskHash
        tcp_bytes = 64MB
        wsl_cpu_sha256 = (($cpuHash -join ' ') -replace "`0", '').Trim()
        wsl_memory_sha256 = (($wslMemoryHash -join ' ') -replace "`0", '').Trim()
        external_sha256 = $externalHash
    }
}

function Get-WinEventsOrEmpty {
    param([hashtable]$Filter)

    try {
        return @(Get-WinEvent -FilterHashtable $Filter -ErrorAction Stop)
    } catch {
        if ($_.FullyQualifiedErrorId -like 'NoMatchingEventsFound,*') {
            return @()
        }
        throw
    }
}

function Get-ErrorCounts {
    param([DateTime]$Start)

    $wer = @(Get-WinEventsOrEmpty -Filter @{
            LogName = 'System'
            StartTime = $Start
            Level = @(1, 2)
            ProviderName = 'Microsoft-Windows-WER-SystemErrorReporting'
        })
    $whea = @(Get-WinEventsOrEmpty -Filter @{
            LogName = 'System'
            StartTime = $Start
            Level = @(1, 2, 3)
            ProviderName = 'Microsoft-Windows-WHEA-Logger'
        })
    $hyperv = @()
    foreach ($log in @(
            'Microsoft-Windows-Hyper-V-Hypervisor-Admin',
            'Microsoft-Windows-Hyper-V-VMMS-Admin'
        )) {
        $hyperv += @(Get-WinEventsOrEmpty -Filter @{
                LogName = $log
                StartTime = $Start
                Level = @(1, 2)
            })
    }
    return [pscustomobject]@{
        hardware = $wer.Count + $whea.Count
        hyperv = $hyperv.Count
    }
}

function Invoke-DailySoak {
    param(
        [string]$Stamp,
        [int]$Minutes,
        [int]$Rounds,
        [string]$ExternalUrl,
        [string]$RunId
    )

    if (-not (Test-Path -LiteralPath $phaseFile)) {
        Assert-Soak ($Minutes -gt 0 -or $Rounds -gt 0) `
            'daily soak needs a positive minute or round target'
        Assert-Soak ($RunId -match `
                '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$') `
            'daily soak needs a valid run ID'
        $externalUri = Get-ExternalUri -Text $ExternalUrl
        $started = [DateTime]::UtcNow
        $phaseOne = Invoke-DailyWorkload -Phase 'phase1' -ExternalUri $externalUri
        [pscustomobject]@{
            start_utc = $started.ToString('o')
            boot_utc = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')
            media_stamp = $Stamp
            run_id = $RunId
            target_minutes = $Minutes
            target_rounds = $Rounds
            external_url = $ExternalUrl
            phase1_disk_sha256 = $phaseOne.disk_sha256
            phase1_external_sha256 = $phaseOne.external_sha256
        } | ConvertTo-Json | Set-Content -LiteralPath $phaseFile -Encoding ASCII
        $lines = @(
            "thin-hv: windows daily soak phase=1 PASS elapsed_ms=$($phaseOne.elapsed_ms)",
            "thin-hv: windows daily soak phase=1 memory_sha256=$($phaseOne.memory_sha256)",
            "thin-hv: windows daily soak phase=1 disk_sha256=$($phaseOne.disk_sha256)",
            "thin-hv: windows daily soak phase=1 tcp_bytes=$($phaseOne.tcp_bytes)",
            "thin-hv: windows daily soak phase=1 wsl_cpu=$($phaseOne.wsl_cpu_sha256)",
            "thin-hv: windows daily soak phase=1 wsl_memory=$($phaseOne.wsl_memory_sha256)"
        )
        if ($phaseOne.external_sha256) {
            $lines += "thin-hv: windows daily soak phase=1 external_sha256=$($phaseOne.external_sha256)"
        }
        Write-Com2 -Lines $lines
        Restart-Computer -Force
        exit 0
    }

    $state = Get-Content -LiteralPath $phaseFile -Raw | ConvertFrom-Json
    Assert-Soak ([string]$state.media_stamp -eq $Stamp) `
        'daily soak media stamp changed during resume'
    Assert-Soak ([string]$state.run_id -match `
            '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$') `
        'daily soak state has an invalid run ID'
    if ($RunId) {
        Assert-Soak ($RunId -eq [string]$state.run_id) `
            'daily soak run ID changed during resume'
    }
    $RunId = [string]$state.run_id
    if ($Minutes -gt 0 -or $Rounds -gt 0) {
        Assert-Soak ($Minutes -eq [int]$state.target_minutes -and `
                $Rounds -eq [int]$state.target_rounds) 'daily soak target changed during resume'
    }
    if ($ExternalUrl) {
        Assert-Soak ($ExternalUrl -eq [string]$state.external_url) `
            'daily soak external URL changed during resume'
    }
    $Minutes = [int]$state.target_minutes
    $Rounds = [int]$state.target_rounds
    $externalUri = Get-ExternalUri -Text ([string]$state.external_url)
    $started = [DateTime]::Parse($state.start_utc).ToUniversalTime()
    $bootUtc = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
    Assert-Soak ($bootUtc -gt [DateTime]::Parse($state.boot_utc).ToUniversalTime()) `
        'Windows reboot was not observed'
    $phaseOnePath = Join-Path $stateDirectory 'daily-soak-phase1.bin'
    $persistedHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $phaseOnePath).Hash.ToLowerInvariant()
    Assert-Soak ($persistedHash -eq $state.phase1_disk_sha256) `
        'phase-1 disk hash did not survive reboot'

    $targetRounds = [Math]::Max(2, $Rounds)
    $completedRounds = 1
    $phaseTwo = $null
    $phaseTwoExternalHash = ''
    do {
        $completedRounds++
        $probeUri = if ($completedRounds -eq 2) { $externalUri } else { $null }
        $phaseTwo = Invoke-DailyWorkload -Phase 'phase2' -ExternalUri $probeUri
        if ($phaseTwo.external_sha256) {
            $phaseTwoExternalHash = $phaseTwo.external_sha256
        }
        Write-Com2 -Lines @(
            "thin-hv: windows daily soak progress rounds=$completedRounds elapsed_ms=$([long]([DateTime]::UtcNow - $started).TotalMilliseconds)"
        )
    } while ($completedRounds -lt $targetRounds -or `
        [DateTime]::UtcNow -lt $started.AddMinutes($Minutes))

    $events = Get-ErrorCounts -Start ([DateTime]::Parse($state.boot_utc).ToLocalTime())
    Assert-Soak ($events.hardware -eq 0) "bugcheck/WHEA events=$($events.hardware)"
    Assert-Soak ($events.hyperv -eq 0) "Hyper-V error events=$($events.hyperv)"
    $elapsedMilliseconds = [long]([DateTime]::UtcNow - $started).TotalMilliseconds
    $lines = @(
        "thin-hv: windows daily soak phase=2 PASS elapsed_ms=$($phaseTwo.elapsed_ms)",
        "thin-hv: windows daily soak phase=2 memory_sha256=$($phaseTwo.memory_sha256)",
        "thin-hv: windows daily soak phase=2 disk_sha256=$($phaseTwo.disk_sha256)",
        "thin-hv: windows daily soak phase=2 tcp_bytes=$($phaseTwo.tcp_bytes)",
        "thin-hv: windows daily soak phase=2 wsl_cpu=$($phaseTwo.wsl_cpu_sha256)",
        "thin-hv: windows daily soak phase=2 wsl_memory=$($phaseTwo.wsl_memory_sha256)",
        "thin-hv: windows daily soak rounds=$completedRounds target_minutes=$Minutes elapsed_ms=$elapsedMilliseconds",
        'thin-hv: windows daily soak reboot=1 disk_persist=1 bugcheck_whea=0 hyperv_errors=0',
        'thin-hv: windows hyperv PASS',
        "thin-hv: windows daily soak PASS stamp=$Stamp run_id=$RunId",
        "thin-hv: windows wsl2 PASS stamp=$Stamp",
        'thin-hv: windows wsl2 PASS'
    )
    if ($externalUri) {
        Assert-Soak ($phaseTwoExternalHash -match '^[0-9a-f]{64}$') `
            'phase-2 external hash evidence is missing'
        $lines = @(
            "thin-hv: windows daily soak external_url=$($externalUri.AbsoluteUri)",
            "thin-hv: windows daily soak phase=1 external_sha256=$($state.phase1_external_sha256)",
            "thin-hv: windows daily soak phase=2 external_sha256=$phaseTwoExternalHash"
        ) + $lines
    }
    Remove-Item -LiteralPath $phaseFile, $phaseOnePath, `
        (Join-Path $stateDirectory 'daily-soak-phase2.bin') -Force
    Set-Content -LiteralPath $statusFile -Value $lines -Encoding ASCII
    Write-Com2 -Lines $lines
    exit 0
}

if ($Message) {
    Write-Com2 -Lines @($Message)
    exit 0
}

$dailySoakRequested = $DailySoakMinutes -gt 0 -or $DailySoakRounds -gt 0 -or `
    (Test-Path -LiteralPath $phaseFile)

try {
    # Preserve the existing Hyper-V marker for later monitor-hyperv runs.
    & (Join-Path $stateDirectory 'hyperv-verify.ps1')

    $stamp = (Get-Content -LiteralPath (
            Join-Path $stateDirectory 'wsl-media-stamp.txt'
        ) -Raw).Trim()
    if ($stamp -notmatch '^[0-9a-f]{64}$') {
        throw "Invalid WSL media stamp: $stamp"
    }

    $version = @(& wsl.exe --version 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "wsl --version failed with status $LASTEXITCODE"
    }

    $distro = 'ThinHvTest'
    $installDirectory = Join-Path $stateDirectory 'wsl\ThinHvTest'
    $rootfs = Join-Path $stateDirectory 'thin-hv-wsl-rootfs.tar'
    $names = @(& wsl.exe --list --quiet 2>&1) |
        ForEach-Object { ([string]$_ -replace "`0", '').Trim() }
    if ($LASTEXITCODE -ne 0) {
        throw "wsl --list failed with status $LASTEXITCODE"
    }
    $retryImport = $true
    if (Test-Path -LiteralPath $statusFile) {
        $retryImport = -not [bool](Select-String -LiteralPath $statusFile `
            -SimpleMatch "thin-hv: windows wsl2 PASS stamp=$stamp" -Quiet)
    }
    if ($names -contains $distro -and $retryImport) {
        # ponytail: ThinHvTest contains no user data; recreate only this test
        # import when its deterministic rootfs stamp changes.
        & wsl.exe --unregister $distro | Out-Null
        if ($LASTEXITCODE -ne 0) {
            throw "wsl --unregister failed with status $LASTEXITCODE"
        }
        $names = @($names | Where-Object { $_ -ne $distro })
    }
    if ($names -notcontains $distro) {
        if (Test-Path -LiteralPath $installDirectory) {
            # ponytail: this removes only a partial test import; add recovery if
            # the harness ever stores user data in this dedicated directory.
            Remove-Item -LiteralPath $installDirectory -Recurse -Force
        }
        New-Item -ItemType Directory -Force -Path $installDirectory | Out-Null
        & wsl.exe --import $distro $installDirectory $rootfs --version 2
        if ($LASTEXITCODE -ne 0) {
            throw "wsl --import failed with status $LASTEXITCODE"
        }
    }

    $list = @(& wsl.exe --list --verbose 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "wsl --list --verbose failed with status $LASTEXITCODE"
    }
    $listText = (($list -join "`n") -replace "`0", '')
    if ($listText -notmatch '(?m)^\s*\*?\s*ThinHvTest\s+\S+\s+2\s*$') {
        throw "ThinHvTest is not WSL2: $listText"
    }

    $probe = @(
        & wsl.exe --distribution $distro --user root --cd / --exec /bin/sh -c `
            '/bin/uname -a; /bin/cat /proc/cpuinfo; /bin/test -s /proc/cpuinfo; echo thin-hv-wsl2-guest-ok' `
            2>&1
    )
    $probeStatus = $LASTEXITCODE
    $probeText = (($probe -join "`n") -replace "`0", '')
    if ($probeStatus -ne 0 -or
        $probeText -notmatch '(?m)^Linux ' -or
        $probeText -notmatch '(?m)^processor\s*:' -or
        $probeText -notmatch '(?m)^thin-hv-wsl2-guest-ok$') {
        throw "WSL2 probe failed with status $probeStatus`: $probeText"
    }

    if ($dailySoakRequested) {
        Invoke-DailySoak -Stamp $stamp -Minutes $DailySoakMinutes `
            -Rounds $DailySoakRounds -ExternalUrl $DailySoakExternalUrl `
            -RunId $DailySoakRunId
    }

    $lines = @('thin-hv: windows wsl2 version') + $version +
        @('thin-hv: windows wsl2 distributions') + $list +
        @('thin-hv: windows wsl2 probe') + $probe +
        @("thin-hv: windows wsl2 PASS stamp=$stamp") +
        @('thin-hv: windows wsl2 PASS')
    Set-Content -LiteralPath $statusFile -Value $lines -Encoding ASCII
    Write-Com2 -Lines $lines
    exit 0
} catch {
    $errorText = $_ | Out-String
    $lines = @($errorText)
    if ($dailySoakRequested) {
        $lines += 'thin-hv: windows daily soak FAIL'
    }
    $lines += 'thin-hv: windows wsl2 FAIL'
    Set-Content -LiteralPath $statusFile -Value $lines -Encoding UTF8
    Write-Com2 -Lines $lines
    exit 1
}
