#requires -Version 5.1
# QEMU evaluation-image test only. Never collect physical licensing information.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$expected = '{"schema":"thin-hv.physical-status.self-test.v1","result":"PASS","hardware_queries":0}'
$serial = [System.IO.Ports.SerialPort]::new('COM2', 115200,
    [System.IO.Ports.Parity]::None, 8, [System.IO.Ports.StopBits]::One)
try {
    $serial.Open()
    $source = Join-Path $PSScriptRoot 'physical-status.ps1'
    $output = @(& "$PSHOME\powershell.exe" -NoProfile -NonInteractive `
        -ExecutionPolicy Bypass -File $source -SelfTest 2>&1)
    if ($LASTEXITCODE -ne 0 -or $output.Count -ne 1 -or
        [string]$output[0] -cne $expected) {
        throw 'Self-test did not return its exact success record and exit status'
    }
    $serial.WriteLine($expected)
    $serial.WriteLine('thin-hv: windows physical-status self-test PASS exit=0')
} catch {
    # Never forward arbitrary child output, exception text, or firmware data.
    if ($serial.IsOpen) {
        $serial.WriteLine('thin-hv: windows physical-status self-test FAIL')
    }
} finally {
    if ($serial.IsOpen) { $serial.Close() }
    $serial.Dispose()
}
& shutdown.exe /s /t 0 /f
