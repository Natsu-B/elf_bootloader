param(
    [switch]$Reset
)

$ErrorActionPreference = 'Stop'
$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$phaseFile = Join-Path $stateDirectory 'hibernate-phase.json'
$statusFile = Join-Path $stateDirectory 'hibernate-status.txt'
$persistFile = Join-Path $stateDirectory 'hibernate-persist.bin'
New-Item -ItemType Directory -Force -Path $stateDirectory | Out-Null
$continuationNonce = $null

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
            foreach ($line in $Lines) { $serial.WriteLine($line) }
            return
        } catch {
            $lastError = $_
        } finally {
            if ($null -ne $serial) {
                if ($serial.IsOpen) { $serial.Close() }
                $serial.Dispose()
            }
        }
        Start-Sleep -Seconds 1
    }
    throw $lastError
}

function Assert-Test {
    param([bool]$Condition, [string]$Failure)
    if (-not $Condition) { throw $Failure }
}

function Read-SecureBootVariable {
    $variable = Get-SecureBootUEFI -Name SecureBoot
    Assert-Test ($null -ne $variable -and $variable.Bytes.Length -ge 1) `
        'SecureBoot UEFI variable read returned no data'
    return [pscustomobject]@{
        hex = ([System.BitConverter]::ToString($variable.Bytes) -replace '-', '').ToLowerInvariant()
        attributes = [string]$variable.Attributes
    }
}

function Invoke-WslProbe {
    param([string]$Phase)

    $probe = @(
        & wsl.exe --distribution ThinHvTest --user root --cd / --exec `
            /bin/sh -c 'uname -r; test -s /proc/cpuinfo; /bin/busybox sha256sum /proc/cpuinfo; echo thin-hv-hibernate-wsl-ok' `
            2>&1
    )
    $status = $LASTEXITCODE
    $text = (($probe -join "`n") -replace "`0", '')
    Assert-Test ($status -eq 0 -and $text -match '(?m)^thin-hv-hibernate-wsl-ok$') `
        "WSL2 $Phase probe failed with status $status`: $text"
}

function Get-ErrorCounts {
    param([DateTime]$Start)

    $bugchecks = @(Get-WinEvent -FilterHashtable @{
            LogName = 'System'
            ProviderName = 'Microsoft-Windows-WER-SystemErrorReporting'
            Id = 1001
            StartTime = $Start
        } -ErrorAction SilentlyContinue)
    $whea = @(Get-WinEvent -FilterHashtable @{
            LogName = 'System'
            ProviderName = 'Microsoft-Windows-WHEA-Logger'
            StartTime = $Start
            Level = @(1, 2)
        } -ErrorAction SilentlyContinue)
    $hyperv = @()
    foreach ($log in @(
            'Microsoft-Windows-Hyper-V-Hypervisor-Admin',
            'Microsoft-Windows-Hyper-V-VMMS-Admin'
        )) {
        $hyperv += @(Get-WinEvent -FilterHashtable @{
                LogName = $log; StartTime = $Start; Level = @(1, 2)
            } -ErrorAction SilentlyContinue)
    }
    return [pscustomobject]@{
        hardware = $bugchecks.Count + $whea.Count
        hyperv = $hyperv.Count
    }
}

try {
    if ($Reset) {
        Remove-Item -LiteralPath $phaseFile, $statusFile, $persistFile `
            -Force -ErrorAction SilentlyContinue
    }

    & powercfg.exe /hibernate on
    Assert-Test ($LASTEXITCODE -eq 0) 'powercfg /hibernate on failed'
    $powerStates = @(& powercfg.exe /availablesleepstates 2>&1)
    Assert-Test ($LASTEXITCODE -eq 0) 'powercfg /availablesleepstates failed'
    $availableText = (($powerStates -join "`n") -split 'The following sleep states are not available', 2)[0]
    Assert-Test ($availableText -match '(?m)^\s+Hibernate\s*$') 'S4 hibernate is unavailable'
    Assert-Test ($availableText -notmatch 'Standby \(S3\)') 'S3 unexpectedly available'

    if (-not ('ThinHvNativePower' -as [type])) {
        Add-Type @'
using System.Runtime.InteropServices;
public static class ThinHvNativePower {
    [DllImport("PowrProf.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool SetSuspendState(
        [MarshalAs(UnmanagedType.Bool)] bool hibernate,
        [MarshalAs(UnmanagedType.Bool)] bool forceCritical,
        [MarshalAs(UnmanagedType.Bool)] bool disableWakeEvent);
}
'@
    }

    if (-not (Test-Path -LiteralPath $phaseFile)) {
        $start = [DateTime]::UtcNow
        $continuationNonce = [Guid]::NewGuid().ToString('N')
        $firmwareBefore = Read-SecureBootVariable
        Invoke-WslProbe -Phase 'before'

        $block = [byte[]]::new(1MB)
        $random = [System.Security.Cryptography.RandomNumberGenerator]::Create()
        $random.GetBytes($block)
        $random.Dispose()
        $stream = [System.IO.File]::Open(
            $persistFile,
            [System.IO.FileMode]::Create,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        try {
            for ($index = 0; $index -lt 16; $index++) {
                $stream.Write($block, 0, $block.Length)
            }
            $stream.Flush($true)
        } finally {
            $stream.Dispose()
        }
        $persistHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $persistFile).Hash.ToLowerInvariant()
        [pscustomobject]@{
            start_utc = $start.ToString('o')
            request_utc = [DateTime]::UtcNow.ToString('o')
            persist_sha256 = $persistHash
            firmware_hex = $firmwareBefore.hex
            firmware_attributes = $firmwareBefore.attributes
            continuation_nonce = $continuationNonce
        } | ConvertTo-Json | Set-Content -LiteralPath $phaseFile -Encoding ASCII

        Write-Com2 -Lines @(
            'thin-hv: windows power state selected=S4 s3_policy=disabled-by-harness',
            "thin-hv: windows hibernate pre PASS disk_sha256=$persistHash",
            "thin-hv: windows hibernate pre firmware=SecureBoot:$($firmwareBefore.hex)",
            'thin-hv: windows hibernate pre wsl2=PASS',
            'thin-hv: windows hibernate request'
        )
        $suspended = [ThinHvNativePower]::SetSuspendState($true, $false, $false)
        Assert-Test $suspended "SetSuspendState(S4) failed win32=$([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }

    $state = Get-Content -LiteralPath $phaseFile -Raw | ConvertFrom-Json
    Assert-Test ($null -ne $continuationNonce -and `
        $continuationNonce -eq [string]$state.continuation_nonce) `
        'hibernate did not resume the original PowerShell process'
    $requestUtc = [DateTime]::Parse($state.request_utc).ToUniversalTime()
    $elapsedSeconds = [math]::Round(([DateTime]::UtcNow - $requestUtc).TotalSeconds, 3)
    Assert-Test ($elapsedSeconds -ge 3) "hibernate returned too quickly: $elapsedSeconds seconds"

    $persistHashAfter = (
        Get-FileHash -Algorithm SHA256 -LiteralPath $persistFile
    ).Hash.ToLowerInvariant()
    Assert-Test ($persistHashAfter -eq $state.persist_sha256) `
        'persistent file hash changed across hibernate'
    $firmwareAfter = Read-SecureBootVariable
    Assert-Test ($firmwareAfter.hex -eq $state.firmware_hex) `
        'SecureBoot UEFI variable changed across hibernate'
    Assert-Test ($firmwareAfter.attributes -eq $state.firmware_attributes) `
        'SecureBoot UEFI variable attributes changed across hibernate'
    Invoke-WslProbe -Phase 'after'

    $events = Get-ErrorCounts -Start ([DateTime]::Parse($state.start_utc).ToLocalTime())
    Assert-Test ($events.hardware -eq 0) "bugcheck/WHEA events=$($events.hardware)"
    Assert-Test ($events.hyperv -eq 0) "Hyper-V error events=$($events.hyperv)"

    $lines = @(
        "thin-hv: windows hibernate resume PASS elapsed_seconds=$elapsedSeconds",
        "thin-hv: windows hibernate disk_persist=1 sha256=$persistHashAfter",
        "thin-hv: windows hibernate firmware_variable=1 SecureBoot=$($firmwareAfter.hex)",
        'thin-hv: windows hibernate wsl2_after=PASS',
        "thin-hv: windows hibernate events bugcheck_whea=$($events.hardware) hyperv_errors=$($events.hyperv)",
        'thin-hv: windows hibernate process_continuation=PASS',
        'thin-hv: windows hibernate PASS state=S4 guest_resume=1'
    )
    Set-Content -LiteralPath $statusFile -Value $lines -Encoding ASCII
    Write-Com2 -Lines $lines
    exit 0
} catch {
    $errorText = $_ | Out-String
    $lines = @($errorText, 'thin-hv: windows hibernate FAIL')
    Set-Content -LiteralPath $statusFile -Value $lines -Encoding UTF8
    Write-Com2 -Lines $lines
    exit 1
}
