# Public tooler installer for Windows PowerShell.
# Usage: iwr -useb https://raw.githubusercontent.com/venturaproject/tooler/main/install.ps1 | iex

$ErrorActionPreference = 'Stop'
$repo = 'venturaproject/tooler'
$installDir = Join-Path $env:LOCALAPPDATA 'tooler'
$artifact = 'tooler-windows-x86_64.exe'

$version = $env:TOOLER_VERSION
if (-not $version) {
    try {
        $release = Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$repo/releases/latest"
        $version = $release.tag_name
    } catch {
        throw "Could not determine the latest tooler release: $_"
    }
}
if ($version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$') {
    throw "Invalid TOOLER_VERSION '$version'"
}

$baseUrl = "https://github.com/$repo/releases/download/$version"
$tmp = Join-Path $env:TEMP "tooler-$([guid]::NewGuid()).exe"
$sums = Join-Path $env:TEMP "tooler-sums-$([guid]::NewGuid()).txt"
try {
    Write-Host "Installing tooler $version (windows/x86_64)..."
    Invoke-WebRequest -UseBasicParsing "$baseUrl/$artifact" -OutFile $tmp
    Invoke-WebRequest -UseBasicParsing "$baseUrl/SHA256SUMS.txt" -OutFile $sums

    $matches = @(Get-Content $sums | Where-Object {
        $_ -match "^([0-9a-fA-F]{64})\s+$([regex]::Escape($artifact))$"
    })
    if ($matches.Count -ne 1) { throw "Checksum manifest has no unique valid entry for $artifact" }
    $expected = ($matches[0] -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $tmp).Hash.ToLowerInvariant()
    if ($expected -ne $actual) { throw "SHA256 verification failed for $artifact" }

    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    Copy-Item $tmp (Join-Path $installDir 'tooler.exe') -Force

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $pathEntries = @($userPath -split ';' | ForEach-Object { $_.TrimEnd('\') })
    if ($pathEntries -notcontains $installDir.TrimEnd('\')) {
        $newPath = if ([string]::IsNullOrWhiteSpace($userPath)) { $installDir } else { "$userPath;$installDir" }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        Write-Host "Added $installDir to your user PATH (restart the terminal to apply)."
    }
    Write-Host "Installed to $(Join-Path $installDir 'tooler.exe')"
    Write-Host 'Done. Run: tooler --help'
} finally {
    Remove-Item $tmp, $sums -ErrorAction SilentlyContinue
}
