[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Query,
    [ValidateSet('auto', 'web', 'x', 'both')][string]$Mode = 'auto',
    [string[]]$AllowedDomains,
    [string[]]$ExcludedDomains,
    [string[]]$AllowedXHandles,
    [string[]]$ExcludedXHandles,
    [string]$FromDate,
    [string]$ToDate,
    [ValidateRange(10, 600)][int]$TimeoutSec = 180,
    [ValidateRange(1, 10)][int]$MaxTurns = 2,
    [ValidateSet('low', 'medium', 'high')][string]$ReasoningEffort = 'medium'
)

$ErrorActionPreference = 'Stop'
$configPath = Join-Path $env:APPDATA 'codex-switch\skills\grok-search\config.json'
if (-not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
    throw 'Grok Search is not configured. Open Codex Switch and save the Grok Search configuration.'
}
$config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
Import-Module (Join-Path $PSScriptRoot 'GrokSearch.psm1') -Force

try {
    $params = @{
        Query = $Query
        Mode = $Mode
        BaseUrl = [string]$config.base_url
        Model = [string]$config.model
        CredentialPath = [Environment]::ExpandEnvironmentVariables([string]$config.credential_path)
        TimeoutSec = $TimeoutSec
        MaxTurns = $MaxTurns
        ReasoningEffort = $ReasoningEffort
    }
    foreach ($name in @('AllowedDomains', 'ExcludedDomains', 'AllowedXHandles', 'ExcludedXHandles', 'FromDate', 'ToDate')) {
        if ($PSBoundParameters.ContainsKey($name)) { $params[$name] = $PSBoundParameters[$name] }
    }
    Invoke-GrokSearch @params | ConvertTo-Json -Depth 20
}
catch {
    [pscustomobject][ordered]@{
        ok = $false
        error = 'Grok Search request failed. Check the configured URL, key, and provider availability.'
    } | ConvertTo-Json -Depth 5
    exit 1
}
