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
$checksumUrl = "$BASE_URL/releases/$version/SHA256SUMS.txt"

Write-Host "Installing tooler $version (windows/x86_64)..."

# ── download ──────────────────────────────────────────────────────────────────
$tmp = Join-Path $env:TEMP "$BIN_NAME.exe"
$sums = Join-Path $env:TEMP "$BIN_NAME-SHA256SUMS.txt"
try {
    Invoke-WebRequest -UseBasicParsing $url -OutFile $tmp
} catch {
    Write-Error "Download failed from $url`nMake sure you are connected to the VPN."
    exit 1
}
try {
    Invoke-WebRequest -UseBasicParsing $checksumUrl -OutFile $sums
    $matches = @(Get-Content $sums | Where-Object { $_ -match "^([0-9a-fA-F]{64})\s+$([regex]::Escape($artifact))$" })
    if ($matches.Count -ne 1) { throw "checksum manifest has no unique valid entry for $artifact" }
    $expected = ($matches[0] -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $tmp).Hash.ToLowerInvariant()
    if ($expected -ne $actual) { throw "checksum verification failed for $artifact" }
} catch {
    Remove-Item $tmp -ErrorAction SilentlyContinue
    Write-Error $_
    exit 1
} finally {
    Remove-Item $sums -ErrorAction SilentlyContinue
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
