[CmdletBinding()]
param(
    [string] $BinaryPath,
    [Parameter(Mandatory = $true)][string] $OutputDirectory,
    [switch] $VerifyOnly
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$manifestName = 'mycodex-auth-host.manifest.json'
$upstream = 'e0f70019b2758f5b6b9a04dd60e4689481a0c0ac'

function Assert-Identity($Identity) {
    if ($Identity.protocolVersion -ne 2 -or $Identity.hostVersion -cne '0.2.0' -or
        $Identity.upstreamRevision -cne $upstream -or
        $Identity.sourceRevision -cnotmatch '^[0-9a-f]{40}$' -or
        $Identity.sourceDirty -isnot [bool] -or
        $Identity.target -cnotmatch '^(x86_64|aarch64)-(pc-windows-msvc|unknown-linux-gnu|apple-darwin)$' -or
        $Identity.sha256 -cnotmatch '^[0-9a-f]{64}$') {
        throw 'Invalid or unsupported authentication core identity.'
    }
}

if (-not [IO.Path]::IsPathRooted($OutputDirectory)) { throw 'OutputDirectory must be absolute.' }
$destination = [IO.Path]::GetFullPath($OutputDirectory)
$manifestPath = Join-Path $destination $manifestName
if ($VerifyOnly) {
    if ($BinaryPath) { throw 'VerifyOnly reads the packaged executable; omit BinaryPath.' }
    if (-not (Test-Path -LiteralPath (Join-Path $destination 'LICENSE') -PathType Leaf)) {
        throw 'Package is missing the upstream LICENSE.'
    }
    if ((Get-Item -LiteralPath $manifestPath).Length -gt 65536) { throw 'Manifest is too large.' }
    $manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
    Assert-Identity $manifest
    $expectedName = if ($manifest.target -like '*-windows-*') { 'mycodex-auth-host.exe' } else { 'mycodex-auth-host' }
    if ($manifest.schemaVersion -ne 1 -or $manifest.executable -cne $expectedName) { throw 'Invalid package manifest.' }
    $packagedBinary = Join-Path $destination $expectedName
    if ((Get-FileHash -LiteralPath $packagedBinary -Algorithm SHA256).Hash.ToLowerInvariant() -cne $manifest.sha256) {
        throw 'Package executable hash does not match the manifest.'
    }
    Write-Output "Verified authentication core package: $destination"
    return
}

if (-not $BinaryPath -or -not [IO.Path]::IsPathRooted($BinaryPath)) { throw 'BinaryPath must be absolute.' }
$license = Join-Path (Split-Path -Parent $PSScriptRoot) 'LICENSE'
if (-not (Test-Path -LiteralPath $license -PathType Leaf)) { throw 'Cannot package without the upstream LICENSE.' }
$binary = (Get-Item -LiteralPath $BinaryPath).FullName
if ((Test-Path -LiteralPath $destination) -and
    (@(Get-ChildItem -LiteralPath $destination -Force).Count -ne 0)) {
    throw 'OutputDirectory must be new or empty; existing packages are never overwritten.'
}
# Read provenance from the built executable, never from a possibly newer checkout.
$identityText = & $binary --version-json
if ($LASTEXITCODE -ne 0) { throw 'Cannot read authentication core identity.' }
$identity = ($identityText -join "`n") | ConvertFrom-Json
Assert-Identity $identity
$hash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
if ($hash -cne $identity.sha256) { throw 'Executable self-reported hash does not match.' }
$name = if ($identity.target -like '*-windows-*') { 'mycodex-auth-host.exe' } else { 'mycodex-auth-host' }
$manifest = [ordered]@{
    schemaVersion = 1
    hostVersion = $identity.hostVersion
    protocolVersion = $identity.protocolVersion
    upstreamRevision = $identity.upstreamRevision
    sourceRevision = $identity.sourceRevision
    sourceDirty = $identity.sourceDirty
    target = $identity.target
    executable = $name
    sha256 = $hash
}
New-Item -ItemType Directory -Path $destination -Force | Out-Null
$packagedBinary = Join-Path $destination $name
Copy-Item -LiteralPath $binary -Destination $packagedBinary
if ((Get-FileHash -LiteralPath $packagedBinary -Algorithm SHA256).Hash.ToLowerInvariant() -cne $hash) {
    throw 'Copied executable hash does not match; manifest was not created.'
}
Copy-Item -LiteralPath $license -Destination (Join-Path $destination 'LICENSE')
[IO.File]::WriteAllText($manifestPath, ($manifest | ConvertTo-Json) + "`n", (New-Object Text.UTF8Encoding($false)))
Write-Output "Packaged authentication core: $destination"
