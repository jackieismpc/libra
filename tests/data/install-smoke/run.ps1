# install.ps1 smoke harness — six scenario families (plan-20260821 A1-05).
#
#   pwsh -NoProfile -File tests/data/install-smoke/run.ps1
#
# Mirrors run.sh: the production installer is COPIED and only its
# clearly-marked trust/origin constants are rewritten (test public key,
# local fixture server); the production file must stay byte-identical.
# The verifier-unavailable scenarios do not exist here: the Ed25519
# verifier ships inside the script (ADR-UP01-06).
#
# Requires: pwsh 7+, python3 (fixture HTTP server).

$ErrorActionPreference = "Stop"

$SmokeDir = $PSScriptRoot
$RepoRoot = Resolve-Path (Join-Path $SmokeDir "../../..")
$Installer = Join-Path $RepoRoot "install.ps1"
$Fixtures = Join-Path $SmokeDir "fixtures"

$Work = Join-Path ([System.IO.Path]::GetTempPath()) "libra-ps1-smoke-$([guid]::NewGuid().ToString('n'))"
New-Item -ItemType Directory -Path $Work -Force | Out-Null
$Server = $null
$OriginalBytes = [IO.File]::ReadAllBytes($Installer)

function Cleanup {
    if ($null -ne $Server -and -not $Server.HasExited) { $Server.Kill() }
    Remove-Item -Recurse -Force $Work -ErrorAction SilentlyContinue
}

function Fail([string]$Message) {
    Write-Error "FAIL: $Message" -ErrorAction Continue
    Cleanup
    exit 1
}

try {
    # ── fixture server ──────────────────────────────────────────────────────
    $DocRoot = Join-Path $Work "docroot"
    Copy-Item -Recurse (Join-Path $Fixtures "tree/*") $DocRoot -Force
    $PortFile = Join-Path $Work "port"
    $serverScript = @"
import http.server, socketserver, sys, os
os.chdir(sys.argv[1])
handler = http.server.SimpleHTTPRequestHandler
handler.log_message = lambda *a, **k: None
socketserver.TCPServer.allow_reuse_address = True
httpd = socketserver.TCPServer(("127.0.0.1", 0), handler)
open(sys.argv[2], "w").write(str(httpd.server_address[1]))
httpd.serve_forever()
"@
    $serverPath = Join-Path $Work "server.py"
    Set-Content -LiteralPath $serverPath -Value $serverScript
    $Server = Start-Process -FilePath "python3" -ArgumentList @($serverPath, $DocRoot, $PortFile) -PassThru -NoNewWindow
    for ($i = 0; $i -lt 20 -and -not (Test-Path $PortFile); $i++) { Start-Sleep -Milliseconds 300 }
    if (-not (Test-Path $PortFile)) { Fail "fixture server did not report a port" }
    $Base = "http://127.0.0.1:$(Get-Content $PortFile)"

    # ── prepared installer copy ─────────────────────────────────────────────
    $copy = Get-Content -Raw $Installer
    function Rewrite([string]$From, [string]$To) {
        $script:copy = $copy
        if (($copy -split [regex]::Escape($From)).Count -ne 2) { Fail "marker drift: $From" }
        $script:copy = $copy.Replace($From, $To)
    }
    Rewrite '$ReleaseManifestKeyId = "libra-release-1"' '$ReleaseManifestKeyId = "libra-release-test-1"'
    Rewrite '$ReleaseManifestPublicKeyHex = "68aa00ea9358d455645010d811d40702b3f67cec4bdff52d3d4fb8107afaeed3"' '$ReleaseManifestPublicKeyHex = "a8a00ded13ddafaad525fabddc13efc717b29ebed50cd6d653196057fa8f8a43"'
    Rewrite '$ReleaseManifestOrigin = "https://download.libra.tools"' ('$ReleaseManifestOrigin = "' + $Base + '"')
    Rewrite '[string]$DownloadBaseUrl = "https://download.libra.tools",' ('[string]$DownloadBaseUrl = "' + $Base + '",')
    $copy = $copy -replace '\$DefaultVersion = "v[0-9][0-9.]*"', '$DefaultVersion = "v9.9.8"'
    $CopyPath = Join-Path $Work "install-copy.ps1"
    Set-Content -LiteralPath $CopyPath -Value $copy

    # ── scenario runner ─────────────────────────────────────────────────────
    $script:ScenariosRun = 0
    function Run-Scenario([string]$Name, [string]$Manifest, [string]$Expect, [string]$ExpectInstalled, [string]$Needle, [hashtable]$ExtraEnv = @{}) {
        $stableDir = Join-Path $DocRoot "libra/releases/stable"
        Remove-Item -LiteralPath (Join-Path $stableDir "manifest-v1.json") -Force -ErrorAction SilentlyContinue
        if ($Manifest -ne "-none-") {
            New-Item -ItemType Directory -Path $stableDir -Force | Out-Null
            Copy-Item (Join-Path $Fixtures $Manifest) (Join-Path $stableDir "manifest-v1.json") -Force
        }

        $home = Join-Path $Work "home-$Name"
        New-Item -ItemType Directory -Path $home -Force | Out-Null
        $installDir = Join-Path $home "bin"

        $envBackup = @{}
        $scenarioEnv = @{
            PROCESSOR_ARCHITECTURE = "AMD64"
            LOCALAPPDATA           = $home
            USERPROFILE            = $home
            TEMP                   = (Join-Path $home "tmp")
            LIBRA_VERSION          = $null
            LIBRA_ALLOW_FALLBACK   = $null
            LIBRA_NO_ALIAS         = "1"
        } + $ExtraEnv
        foreach ($key in $scenarioEnv.Keys) {
            $envBackup[$key] = [Environment]::GetEnvironmentVariable($key)
            [Environment]::SetEnvironmentVariable($key, $scenarioEnv[$key])
        }
        New-Item -ItemType Directory -Path (Join-Path $home "tmp") -Force | Out-Null

        $failed = $false
        $output = ""
        try {
            $output = & pwsh -NoProfile -File $CopyPath -InstallDir $installDir -NoModifyPath 2>&1 | Out-String
        } catch {
            $failed = $true
            $output += $_.Exception.Message
        }
        if ($LASTEXITCODE -ne 0) { $failed = $true }
        foreach ($key in $envBackup.Keys) {
            [Environment]::SetEnvironmentVariable($key, $envBackup[$key])
        }

        if ($Expect -eq "ok" -and $failed) { Fail "${Name}: expected success`n$output" }
        if ($Expect -eq "fail" -and -not $failed) { Fail "${Name}: expected failure`n$output" }
        $installed = Test-Path (Join-Path $installDir "libra.exe")
        if ($ExpectInstalled -eq "yes" -and -not $installed) { Fail "${Name}: expected an installed libra.exe`n$output" }
        if ($ExpectInstalled -eq "no" -and $installed) { Fail "${Name}: a binary landed on a fail-closed path`n$output" }
        if ($output -notmatch [regex]::Escape($Needle)) { Fail "${Name}: output does not mention '$Needle'`n$output" }
        $script:ScenariosRun++
        Write-Host "ok: $Name"
    }

    Run-Scenario "valid" "manifest-valid.json" "ok" "yes" "Signed stable manifest verified"
    Run-Scenario "bad-signature" "manifest-bad-signature.json" "fail" "no" "SIGNATURE VERIFICATION FAILED"
    Run-Scenario "sha-mismatch" "manifest-sha-mismatch.json" "fail" "no" "sha256 mismatch against the SIGNED manifest"
    Run-Scenario "size-mismatch" "manifest-size-mismatch.json" "fail" "no" "size mismatch"
    Run-Scenario "transition-404" "-none-" "fail" "no" "signature chain is not enabled yet"
    Run-Scenario "transition-404-fallback" "-none-" "ok" "yes" "proceeding UNVERIFIED" @{ LIBRA_ALLOW_FALLBACK = "1" }

    # ── production file untouched ───────────────────────────────────────────
    $after = [IO.File]::ReadAllBytes($Installer)
    if (-not [System.Linq.Enumerable]::SequenceEqual($OriginalBytes, $after)) {
        Fail "the production install.ps1 was modified by the harness"
    }
    if ($ScenariosRun -ne 6) { Fail "expected 6 scenarios, ran $ScenariosRun" }
    Write-Host "install.ps1 smoke: all $ScenariosRun scenarios passed"
} finally {
    Cleanup
}
