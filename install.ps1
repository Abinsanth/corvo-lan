# Installs the latest corvo-lan release (https://github.com/Abinsanth/corvo-lan) on Windows:
#
#   irm https://raw.githubusercontent.com/Abinsanth/corvo-lan/release/install.ps1 | iex
#
# Installs to %LOCALAPPDATA%\Programs\corvo-lan, or to $env:CORVO_LAN_INSTALL_DIR
# if set, and adds it to your user PATH. Never needs admin.

# Everything runs from here, so a download cut short by `irm | iex` runs nothing.
function Install-CorvoLan {
    $ErrorActionPreference = 'Stop'
    # The progress bar makes Invoke-WebRequest crawl on Windows PowerShell.
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may not offer TLS 1.2, which GitHub requires.
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'Abinsanth/corvo-lan'
    $installDir = if ($env:CORVO_LAN_INSTALL_DIR) { $env:CORVO_LAN_INSTALL_DIR }
        else { Join-Path $env:LOCALAPPDATA 'Programs\corvo-lan' }
    # Overridable only to test this script against a local copy of a release.
    $baseUrl = if ($env:CORVO_LAN_BASE_URL) { $env:CORVO_LAN_BASE_URL }
        else { "https://github.com/$repo/releases/latest/download" }

    # ARM64 Windows runs the x64 build through emulation.
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($arch -ne 'AMD64' -and $arch -ne 'ARM64') {
        throw "corvo-lan install: no build for a $arch CPU; build from source: https://github.com/$repo"
    }

    $asset = 'corvo-lan-x86_64-pc-windows-msvc.zip'
    $tmp = Join-Path ([IO.Path]::GetTempPath()) "corvo-lan-install-$PID"
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        Write-Output "downloading $asset"
        $zip = Join-Path $tmp $asset
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset" -OutFile $zip
            Invoke-WebRequest -UseBasicParsing -Uri "$baseUrl/$asset.sha256" -OutFile "$zip.sha256"
        } catch {
            throw "corvo-lan install: couldn't download $baseUrl/$asset ($($_.Exception.Message))"
        }
        $expected = ((Get-Content -Raw "$zip.sha256").Trim() -split '\s+')[0]
        $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash
        if ($expected -ne $actual) {
            throw "corvo-lan install: checksum mismatch for $asset; not installing"
        }

        $out = Join-Path $tmp 'out'
        Expand-Archive -Path $zip -DestinationPath $out
        $new = Join-Path $out 'corvo-lan.exe'
        if (-not (Test-Path $new)) { throw "corvo-lan install: the download didn't contain corvo-lan.exe" }

        New-Item -ItemType Directory -Force -Path $installDir | Out-Null
        $exe = Join-Path $installDir 'corvo-lan.exe'
        $old = Join-Path $installDir 'corvo-lan.old.exe'
        # A running exe can't be overwritten or deleted, but it can be renamed.
        Remove-Item -Force -ErrorAction SilentlyContinue $old
        $movedAside = $false
        if (Test-Path $exe) {
            try { Move-Item $exe $old; $movedAside = $true } catch { $movedAside = $false }
        }
        # Copy beside the target and rename, so a failure never leaves half an exe.
        try {
            Copy-Item -Force $new "$exe.new"
            Move-Item -Force "$exe.new" $exe
        } catch {
            Remove-Item -Force -ErrorAction SilentlyContinue "$exe.new"
            if ($movedAside) {
                try { Move-Item -Force $old $exe } catch {
                    throw "corvo-lan install: couldn't write $exe, and the old version is at $old; rename it back to corvo-lan.exe"
                }
            }
            throw "corvo-lan install: couldn't write $exe; close corvo-lan and try again"
        }
    } finally {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $tmp
    }

    Write-Output "installed $(& $exe --version) to $installDir"
    # Read the raw value: the expanded one would freeze %VARS% and turn the
    # value into a plain string when written back.
    $key = Get-Item 'HKCU:\Environment'
    $userPath = $key.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    $entries = @($userPath -split ';' | ForEach-Object { $_.TrimEnd('\') })
    if ($entries -notcontains $installDir.TrimEnd('\')) {
        $newPath = if ($userPath) { "$userPath;$installDir" } else { $installDir }
        Set-ItemProperty -Path 'HKCU:\Environment' -Name Path -Value $newPath -Type ExpandString
        # Set-ItemProperty doesn't tell Explorer the environment changed; these calls do, so new terminals see PATH.
        [Environment]::SetEnvironmentVariable('CORVO_LAN_INSTALLING', '1', 'User')
        [Environment]::SetEnvironmentVariable('CORVO_LAN_INSTALLING', $null, 'User')
        $env:Path = "$env:Path;$installDir"
        Write-Output ''
        Write-Output "added $installDir to your PATH. Open a new terminal, then run: corvo-lan"
    } else {
        Write-Output 'run: corvo-lan'
    }
}

Install-CorvoLan
