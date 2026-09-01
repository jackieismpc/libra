<#
.SYNOPSIS
  Installs Libra on Windows.

.DESCRIPTION
  Default install is per-user and does not require administrator privileges.
  Override the version with -Version or LIBRA_VERSION.
#>

[CmdletBinding()]
param(
    [string]$Version = $env:LIBRA_VERSION,
    [string]$InstallDir = "",
    [string]$DownloadBaseUrl = "https://download.libra.tools",
    [switch]$NoModifyPath,
    # Skip the optional `lba` shorthand. Mirrors install.sh's --no-alias.
    [switch]$NoAlias
)

$ErrorActionPreference = "Stop"

# One of the release version surfaces. `compat_version_surface_sync` pins it
# to Cargo.toml: this value is substituted verbatim into the download URL, so
# a stale value silently installs an old binary when -Version is not given.
$DefaultVersion = "v0.22.5"
# Public-only trust anchor for stable-manifest verification. It deliberately
# has no environment override: the install-smoke harness rewrites these
# clearly-marked constants in a temporary COPY of this script.
$ReleaseManifestKeyId = "libra-release-1"
$ReleaseManifestPublicKeyHex = "68aa00ea9358d455645010d811d40702b3f67cec4bdff52d3d4fb8107afaeed3"
# Pinned origin of the signed stable channel (marker for the smoke harness).
$ReleaseManifestOrigin = "https://download.libra.tools"
$ExeName = "libra.exe"
$ReleaseAsset = "libra-windows-amd64.exe"

# ─── minimal self-contained Ed25519 verifier (RFC 8032, ADR-UP01-06) ─────────
# Windows has no built-in Ed25519 (CNG/.NET both lack it), so the installer
# ships this dependency-free verifier: BigInteger field math + SHA-512 from
# .NET. Verification only — there is no signing capability here.

$Ed = @{
    P  = ([System.Numerics.BigInteger]::Pow(2, 255)) - 19
    L  = ([System.Numerics.BigInteger]::Pow(2, 252)) + [System.Numerics.BigInteger]::Parse("27742317777372353535851937790883648493")
    D  = [System.Numerics.BigInteger]::Parse("37095705934669439343138083508754565189542113879843219016388785533085940283555")
    I  = [System.Numerics.BigInteger]::Parse("19681161376707505956807079304988542015446066515923890162744021073123829784752")
    Gx = [System.Numerics.BigInteger]::Parse("15112221349535400772501151409588531511454012693041857206046113283949847762202")
    Gy = [System.Numerics.BigInteger]::Parse("46316835694926478169428394003475163141307993866256225615783033603165251855960")
}

function Get-Mod([System.Numerics.BigInteger]$a, [System.Numerics.BigInteger]$m) {
    $r = $a % $m
    if ($r -lt 0) { $r += $m }
    return $r
}

function Get-ModPow([System.Numerics.BigInteger]$b, [System.Numerics.BigInteger]$e, [System.Numerics.BigInteger]$m) {
    return [System.Numerics.BigInteger]::ModPow($b, $e, $m)
}

function Get-ModInv([System.Numerics.BigInteger]$a) {
    return Get-ModPow $a ($Ed.P - 2) $Ed.P
}

function ConvertFrom-LittleEndian([byte[]]$Bytes) {
    $padded = New-Object byte[] ($Bytes.Length + 1)
    [Array]::Copy($Bytes, $padded, $Bytes.Length)
    return [System.Numerics.BigInteger]::new($padded)
}

# Extended homogeneous coordinates (X:Y:Z:T), T = XY/Z.
function New-EdPoint($x, $y, $z, $t) { return @{ X = $x; Y = $y; Z = $z; T = $t } }

function Add-EdPoints($p1, $p2) {
    $P = $Ed.P
    $a = Get-Mod (($p1.Y - $p1.X) * ($p2.Y - $p2.X)) $P
    $b = Get-Mod (($p1.Y + $p1.X) * ($p2.Y + $p2.X)) $P
    $c = Get-Mod ((2 * $p1.T) * $p2.T % $P * $Ed.D) $P
    $d = Get-Mod ((2 * $p1.Z) * $p2.Z) $P
    $e = $b - $a; $f = $d - $c; $g = $d + $c; $h = $b + $a
    return New-EdPoint (Get-Mod ($e * $f) $P) (Get-Mod ($g * $h) $P) (Get-Mod ($f * $g) $P) (Get-Mod ($e * $h) $P)
}

function Step-EdDouble($p1) {
    $P = $Ed.P
    $a = Get-ModPow $p1.X 2 $P
    $b = Get-ModPow $p1.Y 2 $P
    $c = Get-Mod (2 * (Get-ModPow $p1.Z 2 $P)) $P
    $h = Get-Mod ($a + $b) $P
    $e = Get-Mod ($h - (Get-ModPow ($p1.X + $p1.Y) 2 $P)) $P
    $g = Get-Mod ($a - $b) $P
    $f = Get-Mod ($c + $g) $P
    return New-EdPoint (Get-Mod ($e * $f) $P) (Get-Mod ($g * $h) $P) (Get-Mod ($f * $g) $P) (Get-Mod ($e * $h) $P)
}

function Get-EdScalarMult([System.Numerics.BigInteger]$k, $point) {
    $q = New-EdPoint ([System.Numerics.BigInteger]::Zero) ([System.Numerics.BigInteger]::One) ([System.Numerics.BigInteger]::One) ([System.Numerics.BigInteger]::Zero)
    $addend = $point
    while ($k -gt 0) {
        if (($k % 2) -eq 1) { $q = Add-EdPoints $q $addend }
        $addend = Step-EdDouble $addend
        $k = $k / 2
    }
    return $q
}

# Decompress a 32-byte point; $null when the encoding is invalid.
function ConvertTo-EdPoint([byte[]]$Bytes) {
    if ($Bytes.Length -ne 32) { return $null }
    $P = $Ed.P
    $signBit = ($Bytes[31] -band 0x80) -ne 0
    $yBytes = [byte[]]$Bytes.Clone()
    $yBytes[31] = $yBytes[31] -band 0x7f
    $y = ConvertFrom-LittleEndian $yBytes
    if ($y -ge $P) { return $null }
    $y2 = Get-Mod ($y * $y) $P
    $u = Get-Mod ($y2 - 1) $P
    $v = Get-Mod (($Ed.D * $y2) + 1) $P
    # x = u * v^3 * (u * v^7)^((p-5)/8)
    $v3 = Get-Mod ((Get-ModPow $v 3 $P)) $P
    $v7 = Get-Mod ((Get-ModPow $v 7 $P)) $P
    $x = Get-Mod ($u * $v3 % $P * (Get-ModPow (Get-Mod ($u * $v7) $P) (($P - 5) / 8) $P)) $P
    $check = Get-Mod ($v * $x * $x) $P
    if ($check -eq (Get-Mod (-$u) $P)) { $x = Get-Mod ($x * $Ed.I) $P }
    elseif ($check -ne (Get-Mod $u $P)) { return $null }
    if (($x -eq 0) -and $signBit) { return $null }
    if (($x % 2 -eq 1) -ne $signBit) { $x = $P - $x }
    return New-EdPoint $x $y ([System.Numerics.BigInteger]::One) (Get-Mod ($x * $y) $P)
}

function Test-EdPointsEqual($p1, $p2) {
    $P = $Ed.P
    if ((Get-Mod ($p1.X * $p2.Z) $P) -ne (Get-Mod ($p2.X * $p1.Z) $P)) { return $false }
    if ((Get-Mod ($p1.Y * $p2.Z) $P) -ne (Get-Mod ($p2.Y * $p1.Z) $P)) { return $false }
    return $true
}

function Test-Ed25519Signature([byte[]]$PublicKey, [byte[]]$Message, [byte[]]$Signature) {
    if ($PublicKey.Length -ne 32 -or $Signature.Length -ne 64) { return $false }
    $A = ConvertTo-EdPoint $PublicKey
    if ($null -eq $A) { return $false }
    $rBytes = [byte[]]($Signature[0..31])
    $R = ConvertTo-EdPoint $rBytes
    if ($null -eq $R) { return $false }
    $s = ConvertFrom-LittleEndian ([byte[]]($Signature[32..63]))
    if ($s -ge $Ed.L) { return $false }

    $sha512 = [System.Security.Cryptography.SHA512]::Create()
    try {
        $hashInput = New-Object byte[] (64 + $Message.Length)
        [Array]::Copy($rBytes, 0, $hashInput, 0, 32)
        [Array]::Copy($PublicKey, 0, $hashInput, 32, 32)
        [Array]::Copy($Message, 0, $hashInput, 64, $Message.Length)
        $k = Get-Mod (ConvertFrom-LittleEndian ($sha512.ComputeHash($hashInput))) $Ed.L
    } finally {
        $sha512.Dispose()
    }

    $B = New-EdPoint $Ed.Gx $Ed.Gy ([System.Numerics.BigInteger]::One) (Get-Mod ($Ed.Gx * $Ed.Gy) $Ed.P)
    $sB = Get-EdScalarMult $s $B
    $kA = Get-EdScalarMult $k $A
    $rPlusKa = Add-EdPoints $R $kA
    return Test-EdPointsEqual $sB $rPlusKa
}

function ConvertFrom-HexString([string]$Hex) {
    $bytes = New-Object byte[] ($Hex.Length / 2)
    for ($i = 0; $i -lt $bytes.Length; $i++) {
        $bytes[$i] = [Convert]::ToByte($Hex.Substring($i * 2, 2), 16)
    }
    return $bytes
}

# ─── signed stable channel (UP-01 A1-05) ─────────────────────────────────────

# Explicit-confirm gate for the transition states (manifest 404; the verifier
# is always available here since it ships with the script). NEVER silent.
function Confirm-UnverifiedTransition([string]$Reason) {
    if ($env:LIBRA_ALLOW_FALLBACK -eq "1") {
        Write-Warning "$Reason - proceeding UNVERIFIED (LIBRA_ALLOW_FALLBACK=1)"
        return
    }
    if ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected) {
        Write-Warning $Reason
        $answer = Read-Host "Continue with an UNVERIFIED download? [y/N]"
        if ($answer -match '^(y|yes)$') {
            Write-Warning "user confirmed UNVERIFIED install"
            return
        }
    }
    throw "$Reason - set LIBRA_ALLOW_FALLBACK=1 (or confirm interactively) to opt in to an UNVERIFIED install"
}

# Resolve the signed stable channel. Returns a hashtable with Version, Url,
# Sha256 and Size on success; $null after an explicitly confirmed transition.
# Every verification failure throws (fail closed).
function Resolve-StableChannel {
    $manifestUrl = "$ReleaseManifestOrigin/libra/releases/stable/manifest-v1.json"
    $raw = $null
    try {
        $ProgressPreference = "SilentlyContinue"
        $response = Invoke-WebRequest -Uri $manifestUrl -UseBasicParsing
        $raw = $response.Content
    } catch {
        $status = $null
        if ($_.Exception.Response) {
            try { $status = [int]$_.Exception.Response.StatusCode } catch { $status = $null }
        }
        if ($status -eq 404) {
            Confirm-UnverifiedTransition "the auto-upgrade signature chain is not enabled yet (stable manifest does not exist)"
            return $null
        }
        throw "could not fetch the signed stable manifest from $manifestUrl : $($_.Exception.Message)"
    }

    if ($raw -is [byte[]]) { $raw = [System.Text.Encoding]::UTF8.GetString($raw) }
    $envelope = $raw | ConvertFrom-Json
    if ($envelope.schema_version -ne 1) { throw "stable manifest has unsupported schema_version '$($envelope.schema_version)'" }
    $signatureEntry = @($envelope.signatures) | Where-Object { $_.key_id -eq $ReleaseManifestKeyId } | Select-Object -First 1
    if ($null -eq $signatureEntry) { throw "stable manifest carries no signature from key '$ReleaseManifestKeyId'" }

    $payloadBytes = [Convert]::FromBase64String([string]$envelope.payload)
    $signatureBytes = [Convert]::FromBase64String([string]$signatureEntry.signature)
    $domain = [System.Text.Encoding]::ASCII.GetBytes("libra-upgrade-manifest-v1")
    $message = New-Object byte[] ($domain.Length + 1 + $payloadBytes.Length)
    [Array]::Copy($domain, 0, $message, 0, $domain.Length)
    $message[$domain.Length] = 0
    [Array]::Copy($payloadBytes, 0, $message, $domain.Length + 1, $payloadBytes.Length)

    $publicKey = ConvertFrom-HexString $ReleaseManifestPublicKeyHex
    if (-not (Test-Ed25519Signature $publicKey $message $signatureBytes)) {
        throw "stable manifest SIGNATURE VERIFICATION FAILED - refusing to install (the download origin may be compromised)"
    }

    $payload = [System.Text.Encoding]::UTF8.GetString($payloadBytes) | ConvertFrom-Json
    if ($payload.channel -ne "stable") { throw "signed manifest channel '$($payload.channel)' is not 'stable'" }
    if ([datetime]::UtcNow -ge ([datetime]::Parse($payload.expires_at, [cultureinfo]::InvariantCulture, [System.Globalization.DateTimeStyles]::AdjustToUniversal))) {
        throw "signed stable manifest is expired (expires_at $($payload.expires_at)) - the publisher must renew it"
    }
    if ($payload.paused -eq $true) {
        throw "releases are PAUSED by the publisher (signed manifest paused=true) - an emergency stop is active"
    }
    if (@($payload.revoked_versions) -contains [string]$payload.version) {
        throw "signed stable version $($payload.version) is REVOKED - refusing to install"
    }
    $artifact = @($payload.artifacts) | Where-Object { $_.platform -eq "windows-amd64" } | Select-Object -First 1
    if ($null -eq $artifact) { throw "signed manifest has no artifact for windows-amd64" }
    if ($artifact.url -notlike "https://download.libra.tools/libra/releases/*") {
        throw "signed artifact URL is outside the pinned origin: $($artifact.url)"
    }
    return @{
        Version = "v$($payload.version)"
        Url     = $ReleaseManifestOrigin + ([string]$artifact.url).Substring("https://download.libra.tools".Length)
        Sha256  = ([string]$artifact.sha256).ToLowerInvariant()
        Size    = [long]$artifact.size
    }
}

# Marker written into every shim this installer generates. It is how a
# reinstall recognises ITS OWN shim (so rewriting is idempotent) and how it
# recognises a file it did not write (so an unrelated `lba` on the user's
# PATH is never clobbered).
$ShimMarker = "@rem libra-managed-shim"

function Write-Info {
    param([string]$Message)
    Write-Host "[libra-install] $Message" -ForegroundColor Cyan
}

function Ensure-Directory {
    param([string]$Path)
    if (-not (Test-Path -LiteralPath $Path)) {
        New-Item -ItemType Directory -Path $Path -Force | Out-Null
    }
}

function Add-UserPath {
    param([string]$PathToAdd)

    if ($NoModifyPath) {
        Write-Info "Skipping PATH update because -NoModifyPath was set."
        return
    }

    $normalizedPath = $PathToAdd.TrimEnd("\")
    $currentUserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $userPathItems = @()

    if (-not [string]::IsNullOrWhiteSpace($currentUserPath)) {
        $userPathItems = $currentUserPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    }

    $alreadyInUserPath = $false
    foreach ($item in $userPathItems) {
        if ($item.TrimEnd("\") -ieq $normalizedPath) {
            $alreadyInUserPath = $true
            break
        }
    }

    if (-not $alreadyInUserPath) {
        $nextUserPath = (@($normalizedPath) + $userPathItems) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $nextUserPath, "User")
        Write-Info "Added to user PATH: $normalizedPath"
    } else {
        Write-Info "PATH already contains: $normalizedPath"
    }

    if (($env:Path -split ";" | ForEach-Object { $_.TrimEnd("\") }) -notcontains $normalizedPath) {
        $env:Path = "$normalizedPath;$env:Path"
    }
}

function Install-CmdShim {
    param(
        [string]$TargetExe,
        [string]$InstallBin
    )

    $shimDirs = @(
        (Join-Path $env:LOCALAPPDATA "Microsoft\WindowsApps"),
        (Join-Path $env:USERPROFILE ".local\bin")
    )

    foreach ($dir in $shimDirs) {
        try {
            Ensure-Directory $dir
            $probe = Join-Path $dir ".libra-write-test"
            Set-Content -LiteralPath $probe -Value "ok" -Encoding ASCII -Force
            Remove-Item -LiteralPath $probe -Force -ErrorAction SilentlyContinue

            $shimPath = Join-Path $dir "libra.cmd"
            Set-Content -LiteralPath $shimPath -Encoding ASCII -Force -Value @(
                "@echo off",
                $ShimMarker,
                "`"$TargetExe`" %*"
            )
            Write-Info "Installed CMD shim: $shimPath"
            return $dir
        } catch {
            continue
        }
    }

    Write-Info "Could not create a CMD shim. Reopen your terminal if libra is not found immediately."
    Write-Info "Manual PATH fallback: set PATH=%PATH%;$InstallBin"
    return $null
}

# The optional `lba` shorthand, the Windows counterpart of install.sh's
# `lba -> libra` symlink (plan-20260714 PD-10).
#
# Safety contract, matching the POSIX installer:
# - an existing `lba.*` this installer did not write is NEVER overwritten,
#   because `lba` is short enough to belong to something else;
# - rewriting our own shim is idempotent, so reinstalling is a no-op;
# - opting out via -NoAlias / LIBRA_NO_ALIAS is honoured;
# - failure to create the shim warns, it does not fail the installation.
function Install-LbaAlias {
    param(
        [string]$TargetExe,
        [string]$ShimDir
    )

    if ($NoAlias -or $env:LIBRA_NO_ALIAS -eq "1") {
        Write-Info "lba alias: disabled"
        return
    }
    if ([string]::IsNullOrWhiteSpace($ShimDir)) {
        Write-Info "lba alias: skipped - no writable shim directory"
        return
    }

    # Any spelling Windows would resolve ahead of, or instead of, our shim.
    foreach ($ext in @(".exe", ".bat", ".ps1")) {
        $rival = Join-Path $ShimDir "lba$ext"
        if (Test-Path -LiteralPath $rival) {
            Write-Info "lba alias: skipped - $rival already exists and was not installed by libra"
            return
        }
    }

    $aliasPath = Join-Path $ShimDir "lba.cmd"
    if (Test-Path -LiteralPath $aliasPath) {
        $existing = ""
        try {
            $existing = Get-Content -LiteralPath $aliasPath -Raw -ErrorAction Stop
        } catch {
            $existing = ""
        }
        if ($existing -notmatch [regex]::Escape($ShimMarker)) {
            Write-Info "lba alias: skipped - $aliasPath already exists and was not installed by libra"
            return
        }
    }

    try {
        Set-Content -LiteralPath $aliasPath -Encoding ASCII -Force -Value @(
            "@echo off",
            $ShimMarker,
            "`"$TargetExe`" %*"
        )
        Write-Info "lba alias: $aliasPath -> $TargetExe"
    } catch {
        Write-Info "lba alias: skipped - could not write $aliasPath"
    }
}

$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -notin @("AMD64", "X64")) {
    throw "Unsupported Windows architecture '$arch'. Libra currently provides a Windows amd64 installer only."
}

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    $InstallDir = Join-Path $env:LOCALAPPDATA "libra\bin"
}

# Default path: the Ed25519-signed stable channel (UP-01 A1-05). An explicit
# -Version / LIBRA_VERSION or a custom -DownloadBaseUrl is an opt-in
# UNVERIFIED path and is warned about loudly.
$DownloadBaseUrl = $DownloadBaseUrl.TrimEnd("/")
$stable = $null
if ([string]::IsNullOrWhiteSpace($Version) -and $DownloadBaseUrl -eq $ReleaseManifestOrigin) {
    $stable = Resolve-StableChannel
} elseif (-not [string]::IsNullOrWhiteSpace($Version)) {
    Write-Warning "-Version pins a historic version: this path is NOT verified against the signed stable manifest"
} else {
    Write-Warning "a custom -DownloadBaseUrl is in use: this path is NOT verified against the signed stable manifest"
}

if ($null -ne $stable) {
    $Version = $stable.Version
    $downloadUrl = $stable.Url
    Write-Info "Signed stable manifest verified (Ed25519, key $ReleaseManifestKeyId)"
} else {
    if ([string]::IsNullOrWhiteSpace($Version)) {
        $Version = $DefaultVersion
    }
    if ($Version -notmatch "^v") {
        $Version = "v$Version"
    }
    $downloadUrl = "$DownloadBaseUrl/libra/releases/$Version/$ReleaseAsset"
}

$tempDir = Join-Path $env:TEMP "libra-install"
$tempExe = Join-Path $tempDir $ReleaseAsset
$targetExe = Join-Path $InstallDir $ExeName

Write-Info "Target version: $Version"
Write-Info "Downloading: $downloadUrl"

Ensure-Directory $tempDir
Ensure-Directory $InstallDir

try {
    $ProgressPreference = "SilentlyContinue"
    Invoke-WebRequest -Uri $downloadUrl -OutFile $tempExe -UseBasicParsing

    if (-not (Test-Path -LiteralPath $tempExe)) {
        throw "Download failed: $downloadUrl"
    }

    if ($null -ne $stable) {
        # Signed path: size and sha256 come from the verified manifest and are
        # MANDATORY - any mismatch aborts before anything is installed.
        $actualSize = (Get-Item -LiteralPath $tempExe).Length
        if ($actualSize -ne $stable.Size) {
            throw "size mismatch (signed manifest says $($stable.Size) bytes, got $actualSize) - refusing to install"
        }
        $actualSha = (Get-FileHash -LiteralPath $tempExe -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actualSha -ne $stable.Sha256) {
            throw "sha256 mismatch against the SIGNED manifest (expected $($stable.Sha256), got $actualSha) - refusing to install"
        }
        Write-Info "sha256 + size match the signed manifest"
    }

    Move-Item -LiteralPath $tempExe -Destination $targetExe -Force
    Write-Info "Installed to: $targetExe"

    Add-UserPath $InstallDir
    $shimDir = Install-CmdShim -TargetExe $targetExe -InstallBin $InstallDir
    Install-LbaAlias -TargetExe $targetExe -ShimDir $shimDir

    Write-Info "Installation complete."
    Write-Info "Run: libra --version"
} finally {
    Remove-Item -LiteralPath $tempDir -Recurse -Force -ErrorAction SilentlyContinue
}
