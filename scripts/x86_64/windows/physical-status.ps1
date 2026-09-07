#requires -Version 5.1
<#
.SYNOPSIS
Prints one compact, read-only Windows validation record as JSON on stdout.
.DESCRIPTION
Run once after each independently verified boot: original firmware, the physical
chainloader, and (only after its prerequisites are satisfied) Direct-VMX L0.
BootLabel is an operator-supplied label, NOT attestation of the active backend.
Keep the original firmware Windows Boot Manager entry available as a bypass.

Licensing uses a projected WMI query for Windows base-product LicenseStatus only.
No licensing methods, key properties, full licensing objects, activation changes,
BCD edits, partition/ESP writes, TPM provisioning, Secure Boot changes, feature
enabling, reboot, or WSL workload are performed. No device identifiers, serial
numbers, firmware contents, or exception messages are included in output.

Confirmation means exactly one Windows base license reports LicenseStatus=1.
It does not correlate an active SKU, establish a backend, test device operation,
or prove that activation survives another boot. Multiple licensed base products
are conservatively reported ambiguous, not silently accepted.

Secure Boot and TPM diagnostics may need an elevated Windows PowerShell session.
Unavailable commands/providers or denied reads remain explicit unknown/error
records. Optional diagnostic failures do not alter activation data or trigger a
repair. Compare PnP counts with the original-firmware baseline; an existing
disabled device is not by itself proof of a hypervisor regression.

Exit codes: 0 = one licensed base product and required OS/CPU/PnP inventory complete;
1 = activation not confirmed; 2 = required non-licensing inventory incomplete.
The script creates no files; redirect stdout only if a status record is wanted.
Use the machine's existing script-signing/execution policy; do not change it here.
.PARAMETER BootLabel
Required operator assertion: firmware, physical-chainload, or direct-vmx.
.PARAMETER SelfTest
Runs only synthetic licensing-policy checks; no Windows/firmware queries.
Executing this mode also parses the complete script using the installed engine.
.EXAMPLE
powershell -NoProfile -File .\physical-status.ps1 -BootLabel firmware
.EXAMPLE
powershell -NoProfile -File .\physical-status.ps1 -BootLabel physical-chainload
.EXAMPLE
powershell -NoProfile -File .\physical-status.ps1 -SelfTest
.LINK
https://learn.microsoft.com/en-us/previous-versions/windows/desktop/sppwmi/softwarelicensingproduct
.LINK
https://learn.microsoft.com/en-us/windows/win32/cimwin32prov/win32-pnpentity
.LINK
https://learn.microsoft.com/en-us/powershell/module/secureboot/confirm-securebootuefi
.LINK
https://learn.microsoft.com/en-us/windows/win32/secprov/win32-tpm-isreadyinformation
#>
[CmdletBinding(DefaultParameterSetName = 'Collect')]
param(
    [Parameter(Mandatory = $true, ParameterSetName = 'Collect')]
    [ValidateSet('firmware', 'physical-chainload', 'direct-vmx')]
    [string]$BootLabel,

    [Parameter(Mandatory = $true, ParameterSetName = 'SelfTest')]
    [switch]$SelfTest
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$licenseQuery = "SELECT LicenseStatus FROM SoftwareLicensingProduct WHERE ApplicationID = '55c92734-d682-4d71-983e-d6ec3f16059f' AND LicenseIsAddon = FALSE"
$maxRecords = 4096

function Get-ActivationSummary {
    param([int[]]$StatusCounts)

    if ($StatusCounts.Count -ne 7) { throw 'Unsupported licensing status layout' }
    $total = 0
    foreach ($count in $StatusCounts) {
        if ($count -lt 0 -or $count -gt 4096) { throw 'Invalid licensing count' }
        $total += $count
    }
    if ($total -gt 4096) { throw 'Licensing inventory exceeds bound' }
    $state = if ($total -eq 0) { 'unknown' }
        elseif ($StatusCounts[1] -gt 1) { 'ambiguous' }
        elseif ($StatusCounts[1] -eq 1) { 'licensed' }
        else { 'not-licensed' }
    return [ordered]@{
        state = $state
        confirmed = ($StatusCounts[1] -eq 1)
        base_products = $total
        licensed_base_products = $StatusCounts[1]
        license_status_counts = @(
            for ($code = 0; $code -lt 7; $code++) {
                if ($StatusCounts[$code] -gt 0) {
                    [ordered]@{ code = $code; count = $StatusCounts[$code] }
                }
            }
        )
    }
}

if ($PSCmdlet.ParameterSetName -eq 'SelfTest') {
    if ($licenseQuery -notmatch "^SELECT LicenseStatus FROM SoftwareLicensingProduct WHERE ApplicationID = '[0-9a-f-]+' AND LicenseIsAddon = FALSE$") {
        throw 'Licensing query must remain status-only'
    }
    foreach ($case in @(
        @{ counts = @(0, 0, 0, 0, 0, 0, 0); state = 'unknown'; confirmed = $false },
        @{ counts = @(100, 1, 0, 0, 0, 0, 0); state = 'licensed'; confirmed = $true },
        @{ counts = @(0, 2, 0, 0, 0, 0, 0); state = 'ambiguous'; confirmed = $false },
        @{ counts = @(0, 0, 1, 1, 1, 1, 1); state = 'not-licensed'; confirmed = $false }
    )) {
        $summary = Get-ActivationSummary -StatusCounts $case.counts
        if ($summary.state -ne $case.state -or $summary.confirmed -ne $case.confirmed) {
            throw 'Licensing policy self-test failed'
        }
    }
    foreach ($counts in @(@(-1, 0, 0, 0, 0, 0, 0), @(4096, 1, 0, 0, 0, 0, 0), @(1, 0))) {
        $rejected = $false
        try { Get-ActivationSummary -StatusCounts $counts | Out-Null }
        catch { $rejected = $true }
        if (-not $rejected) { throw 'Invalid licensing input was accepted' }
    }
    '{"schema":"thin-hv.physical-status.self-test.v1","result":"PASS","hardware_queries":0}'
    exit 0
}

$report = [ordered]@{
    schema = 'thin-hv.physical-status.v1'
    collected_utc = [DateTime]::UtcNow.ToString('o')
    boot_label = [ordered]@{ value = $BootLabel; source = 'operator'; backend_verified = $false }
    activation_scope = 'exactly-one-Windows-base-product-LicenseStatus-1; active-SKU-not-correlated'
    activation = [ordered]@{ state = 'unknown'; confirmed = $false }
    os = [ordered]@{ state = 'unknown'; version = $null; build = $null }
    windows_visible_cpus = [ordered]@{ state = 'unknown'; logical = $null }
    pnp = [ordered]@{ state = 'unknown'; problem_devices = $null; error_codes = @(); record_limit = $maxRecords }
    secure_boot = [ordered]@{ state = 'unavailable'; enabled = $null }
    tpm = [ordered]@{ state = 'unavailable'; present = $null; ready = $null }
    required_inventory_complete = $false
    exit_code = 1
}
$requiredComplete = $true

if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    $report.activation = [ordered]@{ state = 'unavailable'; confirmed = $false; reason = 'requires-Windows' }
    $report | ConvertTo-Json -Depth 6 -Compress
    exit 1
}

try {
    $counts = [int[]]@(0, 0, 0, 0, 0, 0, 0)
    $licenseRecords = 0
    Get-CimInstance -Query $licenseQuery -OperationTimeoutSec 15 -ErrorAction Stop | ForEach-Object {
        $licenseRecords++
        if ($licenseRecords -gt $maxRecords -or $null -eq $_.LicenseStatus -or
            -not ($_.LicenseStatus -is [uint32]) -or $_.LicenseStatus -gt 6) {
            throw 'Invalid or excessive licensing status records'
        }
        $counts[[int]$_.LicenseStatus]++
    }
    $report.activation = Get-ActivationSummary -StatusCounts $counts
} catch {
    $report.activation = [ordered]@{
        state = 'error'; confirmed = $false; error_category = $_.CategoryInfo.Category.ToString()
    }
}

try {
    $os = @(Get-CimInstance -Query 'SELECT Version, BuildNumber FROM Win32_OperatingSystem' -OperationTimeoutSec 15 -ErrorAction Stop | Select-Object -First 2)
    if ($os.Count -ne 1 -or [string]$os[0].Version -notmatch '^\d{1,5}\.\d{1,5}\.\d{1,10}(\.\d{1,10})?$' -or
        [string]$os[0].BuildNumber -notmatch '^\d{1,10}$') { throw 'Invalid OS version inventory' }
    $report.os = [ordered]@{ state = 'ok'; version = [string]$os[0].Version; build = [string]$os[0].BuildNumber }
} catch {
    $requiredComplete = $false
    $report.os = [ordered]@{ state = 'error'; error_category = $_.CategoryInfo.Category.ToString() }
}

try {
    $computer = @(Get-CimInstance -Query 'SELECT NumberOfLogicalProcessors FROM Win32_ComputerSystem' -OperationTimeoutSec 15 -ErrorAction Stop | Select-Object -First 2)
    if ($computer.Count -ne 1 -or $null -eq $computer[0].NumberOfLogicalProcessors -or
        -not ($computer[0].NumberOfLogicalProcessors -is [uint32]) -or
        $computer[0].NumberOfLogicalProcessors -lt 1 -or $computer[0].NumberOfLogicalProcessors -gt 4096) {
        throw 'Invalid Windows-visible CPU count'
    }
    $report.windows_visible_cpus = [ordered]@{ state = 'ok'; logical = [uint32]$computer[0].NumberOfLogicalProcessors }
} catch {
    $requiredComplete = $false
    $report.windows_visible_cpus = [ordered]@{ state = 'error'; error_category = $_.CategoryInfo.Category.ToString() }
}

try {
    $problems = 0
    $codes = @{}
    Get-CimInstance -Query 'SELECT ConfigManagerErrorCode FROM Win32_PnPEntity WHERE ConfigManagerErrorCode <> 0' -OperationTimeoutSec 15 -ErrorAction Stop | ForEach-Object {
        $problems++
        if ($problems -gt $maxRecords -or $null -eq $_.ConfigManagerErrorCode -or
            -not ($_.ConfigManagerErrorCode -is [uint32]) -or $_.ConfigManagerErrorCode -eq 0) {
            throw 'Invalid or excessive PnP error records'
        }
        $code = [string]$_.ConfigManagerErrorCode
        if (-not $codes.ContainsKey($code)) {
            if ($codes.Count -ge 64) { throw 'PnP error code diversity exceeds bound' }
            $codes[$code] = 0
        }
        $codes[$code]++
    }
    $report.pnp = [ordered]@{
        state = 'ok'; problem_devices = $problems; record_limit = $maxRecords
        error_codes = @($codes.GetEnumerator() | Sort-Object { [uint32]$_.Key } | ForEach-Object {
            [ordered]@{ code = [uint32]$_.Key; count = $_.Value }
        })
    }
} catch {
    $requiredComplete = $false
    $report.pnp = [ordered]@{ state = 'error'; error_category = $_.CategoryInfo.Category.ToString(); record_limit = $maxRecords }
}

if (Get-Command -Name Confirm-SecureBootUEFI -ErrorAction SilentlyContinue) {
    try {
        $enabled = Confirm-SecureBootUEFI -ErrorAction Stop
        if (-not ($enabled -is [bool])) { throw 'Secure Boot status is not boolean' }
        $report.secure_boot = [ordered]@{ state = 'ok'; enabled = $enabled }
    } catch {
        $report.secure_boot = [ordered]@{ state = 'error'; enabled = $null; error_category = $_.CategoryInfo.Category.ToString() }
    }
}

try {
    # A projected instance query is sufficient to address the read-only method.
    # Do not fetch owner authorization or serialize the CIM instance/result.
    $tpms = @(Get-CimInstance -Namespace 'root\CIMV2\Security\MicrosoftTpm' -Query 'SELECT IsEnabled_InitialValue FROM Win32_Tpm' -OperationTimeoutSec 15 -ErrorAction Stop | Select-Object -First 2)
    if ($tpms.Count -eq 0) {
        $report.tpm = [ordered]@{ state = 'ok'; present = $false; ready = $false }
    } elseif ($tpms.Count -eq 1) {
        $readiness = Invoke-CimMethod -InputObject $tpms[0] -MethodName IsReadyInformation -OperationTimeoutSec 15 -ErrorAction Stop
        if ($null -eq $readiness.ReturnValue -or $readiness.ReturnValue -ne 0 -or -not ($readiness.IsReady -is [bool])) {
            throw 'TPM read-only readiness query failed'
        }
        $report.tpm = [ordered]@{ state = 'ok'; present = $true; ready = $readiness.IsReady }
    } else {
        $report.tpm = [ordered]@{ state = 'unknown'; present = $null; ready = $null; reason = 'ambiguous-instance-count' }
    }
} catch {
    $report.tpm = [ordered]@{ state = 'error'; present = $null; ready = $null; error_category = $_.CategoryInfo.Category.ToString() }
}

$report.required_inventory_complete = $requiredComplete
$report.exit_code = if (-not $report.activation.confirmed) { 1 } elseif (-not $requiredComplete) { 2 } else { 0 }
$report | ConvertTo-Json -Depth 6 -Compress
exit $report.exit_code
