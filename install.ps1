# One-line installer for memdusa on Windows (x86_64).
#
#   irm https://raw.githubusercontent.com/Krishnacore/memdusa/main/install.ps1 | iex
#
# Environment variables:
#   MEMDUSA_VERSION       release tag to install (default: the latest release, e.g. v0.1.1)
#   MEMDUSA_INSTALL_DIR   install directory (default: $env:LOCALAPPDATA\memdusa\bin)
#
# Runs in a child scope so `irm ... | iex` does not leave variables behind.
& {
    $ErrorActionPreference = 'Stop'

    $repo = 'Krishnacore/memdusa'
    $releaseBase = "https://github.com/$repo/releases"

    function Get-LatestVersion {
        # Follow the /releases/latest redirect instead of calling the API: no
        # token needed, and no unauthenticated API rate limit to worry about.
        $request = [System.Net.HttpWebRequest]::Create("$releaseBase/latest")
        $request.Method = 'GET'
        $request.AllowAutoRedirect = $true
        $request.Timeout = 30000
        $request.UserAgent = 'memdusa-installer'
        try {
            $response = $request.GetResponse()
            try {
                $finalUrl = $response.ResponseUri.AbsoluteUri
            } finally {
                $response.Close()
            }
        } catch {
            throw "Could not determine the latest memdusa release: $($_.Exception.Message)"
        }
        $tag = $finalUrl.TrimEnd('/').Split('/')[-1]
        if ($tag -notmatch '^v\d') {
            throw "Could not determine the latest memdusa release from '$finalUrl'. Set `$env:MEMDUSA_VERSION instead."
        }
        return $tag
    }

    function Download-File([string]$Url, [string]$Path) {
        try {
            Invoke-WebRequest -Uri $Url -OutFile $Path -UseBasicParsing
        } catch {
            throw "Failed to download '$Url'. Check `$env:MEMDUSA_VERSION. $($_.Exception.Message)"
        }
    }

    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) {
        # 32-bit PowerShell on 64-bit Windows reports the native architecture here.
        $arch = $env:PROCESSOR_ARCHITEW6432
    }
    if ($arch -notin @('AMD64', 'x86_64')) {
        throw "Unsupported architecture '$arch'. The Windows installer supports x86_64 only (ARM64 is not published yet); download the archive manually from $releaseBase."
    }
    $target = 'x86_64-pc-windows-msvc'

    $version = $env:MEMDUSA_VERSION
    if ([string]::IsNullOrWhiteSpace($version)) {
        $version = Get-LatestVersion
    } elseif ($version -notmatch '^v') {
        $version = "v$version"
    }

    $installDir = $env:MEMDUSA_INSTALL_DIR
    if ([string]::IsNullOrWhiteSpace($installDir)) {
        $installDir = Join-Path $env:LOCALAPPDATA 'memdusa\bin'
    }

    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('memdusa-install-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp -Force | Out-Null

    try {
        $archive = "memdusa-$version-$target.zip"
        $archivePath = Join-Path $tmp $archive
        $checksumPath = "$archivePath.sha256"
        $baseUrl = "$releaseBase/download/$version"

        Write-Host "Downloading $archive ..."
        Download-File "$baseUrl/$archive" $archivePath
        Download-File "$baseUrl/$archive.sha256" $checksumPath

        Write-Host 'Verifying SHA-256 checksum ...'
        $checksumText = Get-Content -Raw -Path $checksumPath
        if ($checksumText -notmatch '^\s*([0-9a-fA-F]{64})') {
            throw "Invalid checksum file: $checksumPath"
        }
        $expected = $Matches[1].ToUpperInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 -Path $archivePath).Hash
        if ($actual -ne $expected) {
            throw "Checksum mismatch for $archive (expected $expected, got $actual). Aborting."
        }

        Expand-Archive -Path $archivePath -DestinationPath $tmp
        $binary = Join-Path $tmp "memdusa-$version-$target\memdusa.exe"
        if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
            throw "$archive did not contain a memdusa.exe binary"
        }

        New-Item -ItemType Directory -Path $installDir -Force | Out-Null
        $dest = Join-Path $installDir 'memdusa.exe'
        Copy-Item -LiteralPath $binary -Destination $dest -Force
        Write-Host "Installed $dest"

        # Add to the user PATH (persistent) and to this process, if missing.
        $normalized = $installDir.TrimEnd([char]'\')
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        $inUserPath = $false
        if (-not [string]::IsNullOrEmpty($userPath)) {
            foreach ($segment in ($userPath -split ';')) {
                if (-not [string]::IsNullOrWhiteSpace($segment) -and
                    $segment.TrimEnd([char]'\') -ieq $normalized) {
                    $inUserPath = $true
                    break
                }
            }
        }
        if (-not $inUserPath) {
            $newPath = if ([string]::IsNullOrEmpty($userPath)) { $normalized } else { "$userPath;$normalized" }
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Host "Added $normalized to your user PATH. Open a new terminal to pick it up."
        }
        if (-not ($env:Path -split ';' | Where-Object {
            $_ -and $_.TrimEnd([char]'\') -ieq $normalized
        })) {
            $env:Path = "$env:Path;$normalized"
        }

        Write-Host ''
        & $dest --version
        Write-Host ''
        Write-Host 'Connect your harness: memdusa install claude-code'
        Write-Host 'Other harnesses: codex, cursor, antigravity, gemini, opencode, pi, hermes, git.'
    } finally {
        Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
    }
}
