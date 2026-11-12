# Exercise downloads, checksum checks and cleanup without changing the installed app.
$ErrorActionPreference = 'Stop'
$installerScript = Join-Path $PSScriptRoot '../install.ps1'

function Invoke-WebRequest {
    param([switch]$UseBasicParsing, [string]$Uri, [string]$OutFile)
    $script:downloadDir = Split-Path -Parent $OutFile
    if ($Uri.EndsWith('/auto-display.exe')) {
        [IO.File]::WriteAllText($OutFile, 'test executable')
        if ($script:scenario -eq 'download failure') {
            throw 'Simulated download failure.'
        }
    } elseif ($Uri.EndsWith('/SHA256SUMS')) {
        $hash = (Get-FileHash (Join-Path $script:downloadDir 'auto-display.exe') -Algorithm SHA256).Hash
        if ($script:scenario -eq 'wrong checksum') { $hash = '0' * 64 }
        $content = "$hash  auto-display.exe"
        if ($script:scenario -eq 'invalid checksum') { $content = 'invalid checksum file' }
        [IO.File]::WriteAllText($OutFile, $content)
    } else {
        throw "Unexpected download URL: $Uri"
    }
}

function Start-Process {
    param([string]$FilePath, [string]$ArgumentList, [string]$WindowStyle, [switch]$Wait, [switch]$PassThru)
    if (-not (Test-Path -LiteralPath $FilePath) -or $ArgumentList -ne 'install' -or
        $WindowStyle -ne 'Hidden' -or $Wait -or -not $PassThru) {
        throw 'Unexpected installer launch arguments.'
    }
    $script:launchCount++
    $process = [pscustomobject]@{ ExitCode = $null }
    $process | Add-Member -MemberType ScriptMethod -Name WaitForExit -Value {
        $script:waitCount++
        $this.ExitCode = 0
        if ($script:scenario -eq 'install failure') { $this.ExitCode = 1 }
    } -PassThru
}

$cases = @(
    @{ Name = 'success'; Error = $null; Launches = 1 },
    @{ Name = 'wrong checksum'; Error = '*download checksum does not match*'; Launches = 0 },
    @{ Name = 'invalid checksum'; Error = '*checksum file is invalid*'; Launches = 0 },
    @{ Name = 'download failure'; Error = '*Simulated download failure*'; Launches = 0 },
    @{ Name = 'install failure'; Error = '*Installation failed (exit 1)*'; Launches = 1 }
)

foreach ($case in $cases) {
    $script:scenario = $case.Name
    $script:downloadDir = $null
    $script:launchCount = 0
    $script:waitCount = 0
    $failure = $null
    try { . $installerScript } catch { $failure = $_.Exception.Message }
    if ($case.Error) {
        if ($failure -notlike $case.Error) {
            throw "$($case.Name): expected '$($case.Error)', got '$failure'"
        }
    } elseif ($failure) {
        throw "$($case.Name): $failure"
    }
    if ($script:launchCount -ne $case.Launches) {
        throw "$($case.Name): expected $($case.Launches) launches, got $script:launchCount"
    }
    if ($script:waitCount -ne $case.Launches) {
        throw "$($case.Name): did not wait for the installer process"
    }
    if (-not $script:downloadDir -or (Test-Path -LiteralPath $script:downloadDir)) {
        throw "$($case.Name): temporary downloads were not cleaned up"
    }
    Write-Host "PASS: $($case.Name)"
}
