$ErrorActionPreference = 'Stop'
$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$errorFile = Join-Path $stateDirectory 'wsl-enable-error.txt'
$verifierTarget = Join-Path $stateDirectory 'serial-marker.ps1'
New-Item -ItemType Directory -Force -Path $stateDirectory | Out-Null

try {
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'wsl-verify.ps1') `
        -Destination $verifierTarget -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'hyperv-verify.ps1') `
        -Destination (Join-Path $stateDirectory 'hyperv-verify.ps1') -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'thin-hv-wsl-rootfs.tar') `
        -Destination (Join-Path $stateDirectory 'thin-hv-wsl-rootfs.tar') -Force
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'wsl-media-stamp.txt') `
        -Destination (Join-Path $stateDirectory 'wsl-media-stamp.txt') -Force
    & $verifierTarget -Message 'thin-hv: windows wsl2 enable begin'

    $msi = Join-Path $PSScriptRoot 'wsl.2.7.11.0.x64.msi'
    $process = Start-Process -FilePath msiexec.exe `
        -ArgumentList "/i `"$msi`" /qn /norestart" -Wait -PassThru
    if (@(0, 3010) -notcontains $process.ExitCode) {
        throw "WSL MSI failed with status $($process.ExitCode)"
    }

    $feature = Get-WindowsOptionalFeature -Online -FeatureName VirtualMachinePlatform
    if ($feature.State -ne 'Enabled') {
        Enable-WindowsOptionalFeature -Online -FeatureName VirtualMachinePlatform `
            -All -NoRestart | Out-Null
    }
    Restart-Computer -Force
} catch {
    $_ | Out-String | Set-Content -LiteralPath $errorFile -Encoding UTF8
    if (Test-Path -LiteralPath $verifierTarget) {
        & $verifierTarget -Message 'thin-hv: windows wsl2 FAIL'
    }
    exit 1
}
