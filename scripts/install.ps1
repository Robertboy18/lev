$ErrorActionPreference = "Stop"

# Windows release installer. It stays independent of Cargo and verifies the
# downloaded executable before replacing anything in the install directory.
#
# Inputs:
#   LEV_REPOSITORY  GitHub owner/repository (rendered by release CI)
#   LEV_VERSION     Optional tag, with or without the leading "v"
#   LEV_INSTALL_DIR Optional destination directory

# Release CI replaces this placeholder. LEV_REPOSITORY keeps the source
# installer useful for forks and local release testing.
$Repository = if ($env:LEV_REPOSITORY) { $env:LEV_REPOSITORY } else { "@LEV_REPOSITORY@" }
if ($Repository -eq "@LEV_REPOSITORY@") {
    throw "Set LEV_REPOSITORY=owner/repository for an unrendered source installer."
}
if (-not [Environment]::Is64BitOperatingSystem) {
    throw "No released lev binary is available for 32-bit Windows."
}

$Asset = "lev-windows-x86_64.exe"
if ($env:LEV_VERSION) {
    $Version = $env:LEV_VERSION
    if (-not $Version.StartsWith("v")) { $Version = "v$Version" }
    $Base = "https://github.com/$Repository/releases/download/$Version"
} else {
    $Base = "https://github.com/$Repository/releases/latest/download"
}

$Temporary = Join-Path ([IO.Path]::GetTempPath()) ("lev-install-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $Temporary | Out-Null
try {
    $Binary = Join-Path $Temporary $Asset
    $Checksum = "$Binary.sha256"
    # Fetch serially so a failed release request is unambiguous.
    Invoke-WebRequest -UseBasicParsing "$Base/$Asset" -OutFile $Binary
    Invoke-WebRequest -UseBasicParsing "$Base/$Asset.sha256" -OutFile $Checksum

    # Nothing reaches the install directory until its release checksum agrees.
    $Fields = (Get-Content -Raw $Checksum).Trim() -split "\s+"
    if ($Fields.Count -ne 2 -or $Fields[1].TrimStart("*") -ne $Asset) {
        throw "Malformed checksum file."
    }
    $Actual = (Get-FileHash $Binary -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Actual -ne $Fields[0].ToLowerInvariant()) {
        throw "SHA-256 verification failed."
    }

    $Destination = if ($env:LEV_INSTALL_DIR) {
        $env:LEV_INSTALL_DIR
    } else {
        Join-Path $env:LOCALAPPDATA "Programs\lev\bin"
    }
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    # Windows cannot replace a running lev process, so this installer owns the
    # final copy rather than delegating to `lev self update`.
    Copy-Item -Force $Binary (Join-Path $Destination "lev.exe")
    Write-Output "installed lev to $(Join-Path $Destination 'lev.exe')"
} finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $Temporary
}
