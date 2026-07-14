[CmdletBinding()]
param(
    [ValidateSet('generate', 'edit')][string]$Action = 'generate',
    [Parameter(Mandatory)][string]$Prompt,
    [string]$OutputPath = (Join-Path (Get-Location) "image2-$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()).png"),
    [string]$Size = '1024x1024',
    [string]$ImagePath,
    [string]$MaskPath
)

$ErrorActionPreference = 'Stop'
if (-not ('System.Security.Cryptography.ProtectedData' -as [type])) {
    Add-Type -AssemblyName System.Security
}
Add-Type -AssemblyName System.Net.Http

function Get-ConfiguredCredential {
    param([Parameter(Mandatory)][string]$Path)
    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw 'Image2 credential is not configured. Open Codex Switch and save the Image2 configuration.'
    }
    $protected = [Convert]::FromBase64String((Get-Content -LiteralPath $Path -Raw).Trim())
    $plain = [System.Security.Cryptography.ProtectedData]::Unprotect(
        $protected,
        $null,
        [System.Security.Cryptography.DataProtectionScope]::CurrentUser
    )
    try { [Text.Encoding]::UTF8.GetString($plain) }
    finally { [Array]::Clear($plain, 0, $plain.Length) }
}

function Assert-ProviderUrl {
    param([Parameter(Mandatory)][string]$Value)
    $uri = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri)) { throw 'Image2 service URL is invalid.' }
    if ($uri.Scheme -notin @('http', 'https') -or $uri.UserInfo -or $uri.Query -or $uri.Fragment) {
        throw 'Image2 service URL is not allowed.'
    }
    if ($uri.Scheme -eq 'http' -and -not $uri.IsLoopback) { throw 'Image2 service URL must use HTTPS.' }
    $uri.AbsoluteUri.TrimEnd('/')
}

$configPath = Join-Path $env:APPDATA 'codex-switch\skills\image2\config.json'
if (-not (Test-Path -LiteralPath $configPath -PathType Leaf)) {
    throw 'Image2 is not configured. Open Codex Switch and save the Image2 configuration.'
}
$config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
$baseUrl = Assert-ProviderUrl ([string]$config.base_url)
$apiKey = Get-ConfiguredCredential ([string]$config.credential_path)
$handler = [Net.Http.HttpClientHandler]::new()
$handler.AllowAutoRedirect = $false
$client = [Net.Http.HttpClient]::new($handler)
$client.Timeout = [TimeSpan]::FromMinutes(5)
$client.DefaultRequestHeaders.Authorization = [Net.Http.Headers.AuthenticationHeaderValue]::new('Bearer', $apiKey)

try {
    if ($Action -eq 'generate') {
        $payload = [ordered]@{
            model = 'gpt-image-2'; prompt = $Prompt; size = $Size; response_format = 'b64_json'; n = 1
        } | ConvertTo-Json -Compress
        $content = [Net.Http.StringContent]::new($payload, [Text.Encoding]::UTF8, 'application/json')
        $endpoint = "$baseUrl/images/generations"
    }
    else {
        if (-not $ImagePath -or -not (Test-Path -LiteralPath $ImagePath -PathType Leaf)) {
            throw 'Image2 edit requires an existing ImagePath.'
        }
        $content = [Net.Http.MultipartFormDataContent]::new()
        $content.Add([Net.Http.StringContent]::new('gpt-image-2'), 'model')
        $content.Add([Net.Http.StringContent]::new($Prompt), 'prompt')
        $content.Add([Net.Http.StringContent]::new('b64_json'), 'response_format')
        $content.Add([Net.Http.StringContent]::new('1'), 'n')
        $imageBytes = [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $ImagePath))
        $content.Add([Net.Http.ByteArrayContent]::new($imageBytes), 'image', [IO.Path]::GetFileName($ImagePath))
        if ($MaskPath) {
            if (-not (Test-Path -LiteralPath $MaskPath -PathType Leaf)) { throw 'Image2 MaskPath does not exist.' }
            $maskBytes = [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $MaskPath))
            $content.Add([Net.Http.ByteArrayContent]::new($maskBytes), 'mask', [IO.Path]::GetFileName($MaskPath))
        }
        $endpoint = "$baseUrl/images/edits"
    }

    $response = $client.PostAsync($endpoint, $content).GetAwaiter().GetResult()
    if (-not $response.IsSuccessStatusCode) {
        throw "Image2 request failed with HTTP $([int]$response.StatusCode). Check the URL, key, and model access."
    }
    $result = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult() | ConvertFrom-Json
    $encoded = [string]$result.data[0].b64_json
    if (-not $encoded) { throw 'Image2 response did not contain image data.' }
    $resolvedOutput = [IO.Path]::GetFullPath($OutputPath)
    $outputDirectory = Split-Path -Parent $resolvedOutput
    if ($outputDirectory) { New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null }
    $tempOutput = "$resolvedOutput.tmp-$([guid]::NewGuid().ToString('N'))"
    [IO.File]::WriteAllBytes($tempOutput, [Convert]::FromBase64String($encoded))
    Move-Item -LiteralPath $tempOutput -Destination $resolvedOutput -Force
    [pscustomobject]@{ ok = $true; path = $resolvedOutput; action = $Action } | ConvertTo-Json -Compress
}
catch {
    if ($_.Exception.Message -like 'Image2 *') { throw }
    throw 'Image2 request failed. Check the configured URL, key, and provider availability.'
}
finally {
    $apiKey = $null
    if ($imageBytes) { [Array]::Clear($imageBytes, 0, $imageBytes.Length) }
    if ($maskBytes) { [Array]::Clear($maskBytes, 0, $maskBytes.Length) }
    if ($content) { $content.Dispose() }
    $client.Dispose()
    $handler.Dispose()
}
