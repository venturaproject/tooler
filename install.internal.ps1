# tooler installer for Windows (internal)
# Usage: iwr -useb https://internal.empresa.com/cli/install.ps1 | iex

$ErrorActionPreference = 'Stop'

$BASE_URL   = 'https://internal.empresa.com/cli'
$BIN_NAME   = 'tooler'
$INSTALL_DIR = "$env:LOCALAPPDATA\tooler"

# ── resolve version ───────────────────────────────────────────────────────────
$version = $env:TOOLER_VERSION
if (-not $version) {
    try {
        $version = (Invoke-WebRequest -UseBasicParsing "$BASE_URL/version").Content.Trim()
    } catch {
        Write-Error "Could not determine version from $BASE_URL/version`nMake sure you are connected to the VPN."
        exit 1
    }
}

$artifact = "$BIN_NAME-windows-x86_64.exe"
$url      = "$BASE_URL/releases/$version/$artifact"

Write-Host "Installing tooler $version (windows/x86_64)..."

# ── download ──────────────────────────────────────────────────────────────────
$tmp = Join-Path $env:TEMP "$BIN_NAME.exe"
try {
    Invoke-WebRequest -UseBasicParsing $url -OutFile $tmp
} catch {
    Write-Error "Download failed from $url`nMake sure you are connected to the VPN."
    exit 1
}

# ── install ───────────────────────────────────────────────────────────────────
New-Item -ItemType Directory -Force -Path $INSTALL_DIR | Out-Null
Copy-Item $tmp "$INSTALL_DIR\$BIN_NAME.exe" -Force
Remove-Item $tmp

# ── add to PATH if needed ─────────────────────────────────────────────────────
$currentPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if ($currentPath -notlike "*$INSTALL_DIR*") {
    [Environment]::SetEnvironmentVariable('Path', "$currentPath;$INSTALL_DIR", 'User')
    Write-Host "Added $INSTALL_DIR to PATH (restart terminal to apply)"
}

Write-Host ""
Write-Host "Installed to $INSTALL_DIR\$BIN_NAME.exe"
Write-Host "Done. Run: tooler --help"
