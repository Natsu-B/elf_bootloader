$ErrorActionPreference = 'Stop'

$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$markerScript = Join-Path $stateDirectory 'serial-marker.ps1'
New-Item -ItemType Directory -Force -Path $stateDirectory | Out-Null

@'
$lastError = $null
for ($attempt = 0; $attempt -lt 60; $attempt++) {
    $serial = $null
    $sent = $false
    try {
        $serial = [System.IO.Ports.SerialPort]::new(
            'COM1',
            115200,
            [System.IO.Ports.Parity]::None,
            8,
            [System.IO.Ports.StopBits]::One
        )
        $serial.Open()
        $serial.WriteLine('thin-hv: windows desktop')
        $sent = $true
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
    if ($sent) {
        Set-Content -LiteralPath "$env:ProgramData\ThinHv\ready.txt" -Value 'thin-hv: windows desktop'
        exit 0
    }
    Start-Sleep -Seconds 1
}
$lastError | Out-String | Set-Content -LiteralPath "$env:ProgramData\ThinHv\serial-error.txt"
exit 1
'@ | Set-Content -LiteralPath $markerScript -Encoding UTF8

$action = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$markerScript`""
$trigger = New-ScheduledTaskTrigger -AtLogOn -User "$env:COMPUTERNAME\thin"
$principal = New-ScheduledTaskPrincipal -UserId "$env:COMPUTERNAME\thin" -LogonType Interactive -RunLevel Highest
Register-ScheduledTask -TaskName 'ThinHvSerialMarker' -Action $action -Trigger $trigger -Principal $principal -Force | Out-Null

& $markerScript
