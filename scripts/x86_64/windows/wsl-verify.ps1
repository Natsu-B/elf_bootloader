param(
    [string]$Message = ''
)

$ErrorActionPreference = 'Stop'
$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$statusFile = Join-Path $stateDirectory 'wsl-status.txt'
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

if ($Message) {
    Write-Com2 -Lines @($Message)
    exit 0
}

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
    $lines = @($errorText, 'thin-hv: windows wsl2 FAIL')
    Set-Content -LiteralPath $statusFile -Value $lines -Encoding UTF8
    Write-Com2 -Lines $lines
    exit 1
}
