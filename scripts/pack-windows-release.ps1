[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $UpxPath,

    [string] $SourceExe = 'src-tauri/target/release/codex-switch.exe',

    [string] $OutputExe = 'release/codex-switch.exe'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$expectedUpxVersion = '5.2.0'
$expectedUpxSha256 = 'F4C0CC7ACA0F1FF0D0B750E966B44139F2FA1A2DB7281F48FC52194400712E1D'
$maximumReleaseBytes = 3000000L
$repoRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$releaseContract = Join-Path $PSScriptRoot 'check-release-contract.mjs'

function Resolve-RepoPath {
    param(
        [Parameter(Mandatory = $true)]
        [string] $Path
    )

    if ([System.IO.Path]::IsPathFullyQualified($Path)) {
        return [System.IO.Path]::GetFullPath($Path)
    }

    return [System.IO.Path]::GetFullPath((Join-Path $repoRoot $Path))
}

function Invoke-NativeCommand {
    param(
        [Parameter(Mandatory = $true)]
        [string] $FilePath,

        [Parameter(Mandatory = $true)]
        [string[]] $ArgumentList,

        [Parameter(Mandatory = $true)]
        [string] $Description
    )

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

$resolvedUpxPath = if ([System.IO.Path]::IsPathFullyQualified($UpxPath)) {
    [System.IO.Path]::GetFullPath($UpxPath)
} else {
    [System.IO.Path]::GetFullPath((Join-Path (Get-Location).Path $UpxPath))
}
$resolvedSourceExe = Resolve-RepoPath -Path $SourceExe
$resolvedOutputExe = Resolve-RepoPath -Path $OutputExe

if (-not (Test-Path -LiteralPath $resolvedUpxPath -PathType Leaf)) {
    throw "UPX executable does not exist: $resolvedUpxPath"
}
if (-not (Test-Path -LiteralPath $resolvedSourceExe -PathType Leaf)) {
    throw "Uncompressed release executable does not exist: $resolvedSourceExe"
}
if (
    [string]::Equals(
        $resolvedSourceExe,
        $resolvedOutputExe,
        [System.StringComparison]::OrdinalIgnoreCase
    )
) {
    throw 'OutputExe must be a copy path, not the uncompressed SourceExe'
}
if (
    [System.IO.Path]::GetFileName($resolvedOutputExe) -cne 'codex-switch.exe'
) {
    throw 'OutputExe must retain the release asset name codex-switch.exe'
}

$actualUpxSha256 = (Get-FileHash -LiteralPath $resolvedUpxPath -Algorithm SHA256).Hash
if ($actualUpxSha256 -ne $expectedUpxSha256) {
    throw "UPX SHA-256 $actualUpxSha256 does not match pinned $expectedUpxSha256"
}

$upxVersionOutput = & $resolvedUpxPath --version 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "UPX version check failed with exit code $LASTEXITCODE"
}
if (($upxVersionOutput | Select-Object -First 1) -notmatch "^upx $([regex]::Escape($expectedUpxVersion))$") {
    throw "UPX version is not pinned $expectedUpxVersion"
}

$nodeCommand = Get-Command node -CommandType Application -ErrorAction Stop |
    Select-Object -First 1
$sourceFile = Get-Item -LiteralPath $resolvedSourceExe
$sourceLength = $sourceFile.Length
$sourceSha256 = (Get-FileHash -LiteralPath $resolvedSourceExe -Algorithm SHA256).Hash
Invoke-NativeCommand `
    -FilePath $nodeCommand.Source `
    -ArgumentList @($releaseContract, $resolvedSourceExe) `
    -Description 'Uncompressed release contract'
if ((Get-FileHash -LiteralPath $resolvedSourceExe -Algorithm SHA256).Hash -ne $sourceSha256) {
    throw 'Uncompressed release executable changed during contract validation'
}

$outputDirectory = Split-Path -Parent $resolvedOutputExe
[System.IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
$stagingExe = Join-Path $outputDirectory "codex-switch.$([guid]::NewGuid().ToString('N')).exe"

try {
    Copy-Item -LiteralPath $resolvedSourceExe -Destination $stagingExe
    if ((Get-FileHash -LiteralPath $stagingExe -Algorithm SHA256).Hash -ne $sourceSha256) {
        throw 'Uncompressed release executable changed while creating the packing copy'
    }
    Invoke-NativeCommand `
        -FilePath $resolvedUpxPath `
        -ArgumentList @('--best', '--lzma', $stagingExe) `
        -Description 'UPX packing'
    Invoke-NativeCommand `
        -FilePath $resolvedUpxPath `
        -ArgumentList @('-t', $stagingExe) `
        -Description 'UPX integrity test'

    $packedSize = (Get-Item -LiteralPath $stagingExe).Length
    if ($packedSize -gt $maximumReleaseBytes) {
        throw "Packed release is $packedSize bytes; hard cap is $maximumReleaseBytes bytes"
    }

    Invoke-NativeCommand `
        -FilePath $nodeCommand.Source `
        -ArgumentList @($releaseContract, $stagingExe) `
        -Description 'Packed release contract'

    [System.IO.File]::Move($stagingExe, $resolvedOutputExe, $true)
    $stagingExe = $null
} finally {
    if ($null -ne $stagingExe -and (Test-Path -LiteralPath $stagingExe)) {
        Remove-Item -LiteralPath $stagingExe -Force
    }
}

$packedFile = Get-Item -LiteralPath $resolvedOutputExe
$packedSha256 = (Get-FileHash -LiteralPath $packedFile.FullName -Algorithm SHA256).Hash

Write-Output "Uncompressed: $sourceLength bytes, SHA-256 $sourceSha256"
Write-Output "Packed: $($packedFile.Length) bytes, SHA-256 $packedSha256"
Write-Output "Release asset: $($packedFile.FullName)"
