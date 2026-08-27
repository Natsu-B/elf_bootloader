$ErrorActionPreference = 'Stop'
$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$errorFile = Join-Path $stateDirectory 'hyperv-enable-error.txt'
$verifierSource = Join-Path $PSScriptRoot 'hyperv-verify.ps1'
$verifierTarget = Join-Path $stateDirectory 'serial-marker.ps1'
New-Item -ItemType Directory -Force -Path $stateDirectory | Out-Null

try {
    Copy-Item -LiteralPath $verifierSource -Destination $verifierTarget -Force
    & $verifierTarget -Message 'thin-hv: windows hyperv enable begin'

    $feature = Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V
    $present = [bool](Get-CimInstance Win32_ComputerSystem).HypervisorPresent
    if ($feature.State -eq 'Enabled' -and $present) {
        & $verifierTarget
        exit $LASTEXITCODE
    }

    if ($feature.State -ne 'Enabled') {
        Enable-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V -All -NoRestart |
            Out-Null
    }
    & bcdedit.exe /set hypervisorlaunchtype Auto | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "bcdedit failed with status $LASTEXITCODE"
    }
    Restart-Computer -Force
} catch {
    $_ | Out-String | Set-Content -LiteralPath $errorFile -Encoding UTF8
    if (Test-Path -LiteralPath $verifierTarget) {
        & $verifierTarget -Message 'thin-hv: windows hyperv enable FAIL'
    }
    exit 1
}
