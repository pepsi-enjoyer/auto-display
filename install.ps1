# Download and install the latest Windows release. Run again to update.
# irm https://raw.githubusercontent.com/pepsi-enjoyer/auto-display/master/install.ps1 | iex
& {
    $ErrorActionPreference = 'Stop'
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw 'auto-display requires Windows.'
    }
    if (-not [Environment]::Is64BitOperatingSystem) {
        throw 'auto-display requires 64-bit Windows.'
    }

    $downloadDir = Join-Path ([IO.Path]::GetTempPath()) ('auto-display-' + [Guid]::NewGuid().ToString('N'))
    $exe = Join-Path $downloadDir 'auto-display.exe'
    $checksums = Join-Path $downloadDir 'SHA256SUMS'
    [IO.Directory]::CreateDirectory($downloadDir) | Out-Null
    try {
        $release = 'https://github.com/pepsi-enjoyer/auto-display/releases/latest/download'
        Write-Host 'Downloading auto-display...'
        Invoke-WebRequest -UseBasicParsing -Uri "$release/auto-display.exe" -OutFile $exe
        Invoke-WebRequest -UseBasicParsing -Uri "$release/SHA256SUMS" -OutFile $checksums

        $checksum = (Get-Content -LiteralPath $checksums -Raw).Trim()
        if ($checksum -notmatch '^([0-9a-fA-F]{64})\s+\*?auto-display\.exe$') {
            throw 'The release checksum file is invalid.'
        }
        $expected = $Matches[1]
        if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ne $expected) {
            throw 'The download checksum does not match. Run the install command again.'
        }

        $installer = Start-Process -FilePath $exe -ArgumentList install -WindowStyle Hidden -Wait -PassThru
        if ($installer.ExitCode -ne 0) {
            throw "Installation failed (exit $($installer.ExitCode)). See %LOCALAPPDATA%\auto-display\auto-display.log."
        }
        Write-Host "Installed to $env:LOCALAPPDATA\auto-display. Running now and at every sign-in."
    }
    finally {
        Remove-Item -LiteralPath $exe, $checksums -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $downloadDir -Force
    }
}
