param(
    [ValidateRange(0, 10080)]
    [int]$Minutes = 60,
    [ValidateRange(0, 10000)]
    [int]$Rounds = 2
)

$ErrorActionPreference = 'Stop'
$stateDirectory = Join-Path $env:ProgramData 'ThinHv'
$verifier = Join-Path $stateDirectory 'serial-marker.ps1'
$externalUrlFile = Join-Path $PSScriptRoot 'daily-soak-external-url.txt'
$runIdFile = Join-Path $PSScriptRoot 'daily-soak-run-id.txt'

Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'wsl-verify.ps1') `
    -Destination $verifier -Force
$externalUrl = if (Test-Path -LiteralPath $externalUrlFile) {
    (Get-Content -LiteralPath $externalUrlFile -Raw).Trim()
} else {
    ''
}
$runId = (Get-Content -LiteralPath $runIdFile -Raw).Trim()
if ($runId -notmatch '^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$') {
    throw "Invalid daily-soak run ID: $runId"
}
& $verifier -DailySoakMinutes $Minutes -DailySoakRounds $Rounds `
    -DailySoakExternalUrl $externalUrl -DailySoakRunId $runId
