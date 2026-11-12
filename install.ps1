# claude-session-sync installer / updater.
#
#   irm https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.ps1 | iex
#
# Downloads the latest release, checks its SHA-256, and runs "claude-session-sync install".
# Run it again at any time to update: your settings are kept.
# To set options (for example --delete-sync), pass them to the install:
#
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/ssgardaf/claude-session-sync/main/install.ps1))) --delete-sync
#
# Nothing here needs administrator rights. Everything runs in its own scope, so your
# PowerShell session is left as it was.
& {
    $ErrorActionPreference = 'Stop'
    $repo  = 'ssgardaf/claude-session-sync'
    $asset = 'claude-session-sync-windows-x64.zip'

    if ($env:OS -ne 'Windows_NT') { throw 'This installer is for Windows. On macOS and Linux use install.sh (see the README).' }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $base = "https://github.com/$repo/releases/latest/download"
    $work = Join-Path ([IO.Path]::GetTempPath()) ('claude-session-sync-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Force $work | Out-Null
    try {
        $zip = Join-Path $work $asset
        Write-Host "Downloading $asset ..."
        Invoke-WebRequest -Uri "$base/$asset" -OutFile $zip -UseBasicParsing
        Invoke-WebRequest -Uri "$base/$asset.sha256" -OutFile "$zip.sha256" -UseBasicParsing

        # Guards against a damaged or cut-off download. It is not a signature: the checksum
        # comes from the same release as the file.
        $want = ((Get-Content "$zip.sha256" -Raw).Trim() -split '\s+')[0].ToLower()
        $have = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLower()
        if ($want -ne $have) { throw "Checksum mismatch: expected $want, got $have. Nothing was installed." }

        Expand-Archive -Path $zip -DestinationPath (Join-Path $work 'x') -Force
        $exe = Join-Path $work 'x\claude-session-sync.exe'
        if (-not (Test-Path $exe)) { throw 'The download does not contain claude-session-sync.exe. Nothing was installed.' }

        Write-Host ("Installing claude-session-sync " + (& $exe --version) + " ...")
        & $exe install @args
        if ($LASTEXITCODE -ne 0) { throw "install failed (exit code $LASTEXITCODE)" }

        Write-Host ''
        Write-Host 'Next: quit the Claude app completely (tray icon too) and open it again.'
        Write-Host 'Then check on it any time with:  claude-session-sync status   (in a new terminal)'
    }
    finally {
        Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
    }
} @args
