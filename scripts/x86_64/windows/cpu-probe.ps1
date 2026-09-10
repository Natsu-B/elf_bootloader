#requires -Version 5.1
# Disposable QEMU fixture: test execution, not firmware identity or licensing.
param([ValidateSet(1, 2, 4, 8)][int]$ExpectedProcessors)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$serial = [System.IO.Ports.SerialPort]::new('COM2', 115200,
    [System.IO.Ports.Parity]::None, 8, [System.IO.Ports.StopBits]::One)
try {
    $serial.Open()
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Threading;
public static class ThinHvCpuProbe {
    [DllImport("kernel32.dll")]
    private static extern uint GetActiveProcessorCount(ushort group);
    [DllImport("kernel32.dll")]
    private static extern uint GetCurrentProcessorNumber();
    [DllImport("kernel32.dll")]
    private static extern IntPtr GetCurrentThread();
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern UIntPtr SetThreadAffinityMask(IntPtr thread, UIntPtr mask);

    public static uint Run(int expected) {
        if (IntPtr.Size != 8 || (expected != 1 && expected != 2 && expected != 4 && expected != 8)
            || GetActiveProcessorCount(0xffff) != expected
            || GetActiveProcessorCount(0) != expected)
            throw new InvalidOperationException("CPU topology mismatch");
        uint visited = 0;
        UIntPtr original = UIntPtr.Zero;
        Thread.BeginThreadAffinity();
        try {
            IntPtr thread = GetCurrentThread();
            uint reference = 0;
            for (int cpu = 0; cpu < expected; cpu++) {
                UIntPtr previous = SetThreadAffinityMask(thread, new UIntPtr(1UL << cpu));
                if (previous == UIntPtr.Zero)
                    throw new InvalidOperationException("Cannot set CPU affinity");
                if (cpu == 0) original = previous;
                Thread.Sleep(1);
                if (GetCurrentProcessorNumber() != cpu)
                    throw new InvalidOperationException("CPU affinity not applied");
                uint value = 0x12345678;
                for (int round = 0; round < 100000; round++)
                    value = unchecked((value ^ (uint)round) * 1664525 + 1013904223);
                if (GetCurrentProcessorNumber() != cpu || (cpu != 0 && value != reference))
                    throw new InvalidOperationException("CPU execution mismatch");
                reference = value;
                visited |= 1U << cpu;
            }
        } finally {
            try {
                if (original != UIntPtr.Zero && SetThreadAffinityMask(GetCurrentThread(), original) == UIntPtr.Zero)
                    throw new InvalidOperationException("Cannot restore CPU affinity");
            } finally { Thread.EndThreadAffinity(); }
        }
        return visited;
    }
}
'@
    $visited = [ThinHvCpuProbe]::Run($ExpectedProcessors)
    if ($visited -ne ((1 -shl $ExpectedProcessors) - 1)) {
        throw 'Incomplete CPU execution coverage'
    }
    $serial.WriteLine("thin-hv: windows SMP PASS cpus=$ExpectedProcessors mask=$visited")
} catch {
    # Do not forward arbitrary Windows exception strings or system information.
    if ($serial.IsOpen) { $serial.WriteLine('thin-hv: windows SMP FAIL') }
    exit 1
} finally {
    if ($serial.IsOpen) { $serial.Close() }
    $serial.Dispose()
}
