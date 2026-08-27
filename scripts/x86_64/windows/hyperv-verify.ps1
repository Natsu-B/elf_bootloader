param(
    [string]$Message = ''
)

$ErrorActionPreference = 'Stop'
$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$statusFile = Join-Path $stateDirectory 'hyperv-status.txt'
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
    Set-Content -LiteralPath $statusFile -Value $Message -Encoding ASCII
    Write-Com2 -Lines @($Message)
    exit 0
}

$featureState = 'Unknown'
$hypervisorPresent = $false
$vmmsStatus = 'Missing'
for ($attempt = 0; $attempt -lt 180; $attempt++) {
    try {
        $featureState = [string](
            Get-WindowsOptionalFeature -Online -FeatureName Microsoft-Hyper-V
        ).State
        $hypervisorPresent = [bool](
            Get-CimInstance Win32_ComputerSystem
        ).HypervisorPresent
        $vmms = Get-Service -Name vmms -ErrorAction SilentlyContinue
        $vmmsStatus = if ($null -eq $vmms) { 'Missing' } else { [string]$vmms.Status }
        if ($featureState -eq 'Enabled' -and $hypervisorPresent -and $vmmsStatus -eq 'Running') {
            break
        }
    } catch {
        # Services and CIM can be unavailable briefly during startup.
    }
    Start-Sleep -Seconds 1
}

$event2 = $null -ne (Get-WinEvent -FilterHashtable @{
        ProviderName = 'Microsoft-Windows-Hyper-V-Hypervisor'
        Id = 2
        StartTime = (Get-Date).AddMinutes(-10)
    } -MaxEvents 1 -ErrorAction SilentlyContinue)
$detail = "thin-hv: windows hyperv feature=$featureState present=$([int]$hypervisorPresent) vmms=$vmmsStatus event2=$([int]$event2)"
$passed = $featureState -eq 'Enabled' -and $hypervisorPresent -and $vmmsStatus -eq 'Running'
$marker = if ($passed) {
    'thin-hv: windows hyperv PASS'
} else {
    'thin-hv: windows hyperv FAIL'
}
Set-Content -LiteralPath $statusFile -Value @($detail, $marker) -Encoding ASCII
Write-Com2 -Lines @($detail, $marker)
if (-not $passed) {
    exit 1
}
