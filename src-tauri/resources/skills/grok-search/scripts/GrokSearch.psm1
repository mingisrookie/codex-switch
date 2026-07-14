$ErrorActionPreference = 'Stop'
if (-not ('System.Security.Cryptography.ProtectedData' -as [type])) {
    Add-Type -AssemblyName System.Security
}

function New-GrokSearchRequest {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Query,
        [ValidateSet('auto', 'web', 'x', 'both')][string]$Mode = 'auto',
        [string]$Model = 'grok-4.5',
        [string[]]$AllowedDomains,
        [string[]]$ExcludedDomains,
        [string[]]$AllowedXHandles,
        [string[]]$ExcludedXHandles,
        [string]$FromDate,
        [string]$ToDate,
        [ValidateRange(1, 10)][int]$MaxTurns = 2,
        [ValidateSet('low', 'medium', 'high')][string]$ReasoningEffort = 'medium'
    )

    if ($AllowedDomains -and $ExcludedDomains) {
        throw 'AllowedDomains and ExcludedDomains cannot be used together.'
    }
    if ($AllowedXHandles -and $ExcludedXHandles) {
        throw 'AllowedXHandles and ExcludedXHandles cannot be used together.'
    }
    if (@($AllowedDomains).Count -gt 5 -or @($ExcludedDomains).Count -gt 5) {
        throw 'Web domain filters support at most 5 domains.'
    }
    if (@($AllowedXHandles).Count -gt 20 -or @($ExcludedXHandles).Count -gt 20) {
        throw 'X handle filters support at most 20 handles.'
    }

    $tools = [Collections.Generic.List[object]]::new()
    if ($Mode -in @('auto', 'web', 'both')) {
        $web = [ordered]@{ type = 'web_search' }
        if ($AllowedDomains) {
            $web.filters = [ordered]@{ allowed_domains = @($AllowedDomains) }
        }
        elseif ($ExcludedDomains) {
            $web.filters = [ordered]@{ excluded_domains = @($ExcludedDomains) }
        }
        $tools.Add($web)
    }
    if ($Mode -in @('auto', 'x', 'both')) {
        $x = [ordered]@{ type = 'x_search' }
        if ($AllowedXHandles) { $x.allowed_x_handles = @($AllowedXHandles) }
        if ($ExcludedXHandles) { $x.excluded_x_handles = @($ExcludedXHandles) }
        if ($FromDate) { $x.from_date = $FromDate }
        if ($ToDate) { $x.to_date = $ToDate }
        $tools.Add($x)
    }

    [ordered]@{
        model = $Model
        input = $Query
        tools = @($tools)
        max_turns = $MaxTurns
        reasoning = [ordered]@{ effort = $ReasoningEffort }
    }
}

function Save-GrokSearchCredential {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$ApiKey,
        [Parameter(Mandatory)][string]$Path
    )

    $directory = Split-Path -Parent $Path
    if ($directory) {
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
    }
    $plainBytes = [Text.Encoding]::UTF8.GetBytes($ApiKey)
    try {
        $protectedBytes = [System.Security.Cryptography.ProtectedData]::Protect(
            $plainBytes,
            $null,
            [System.Security.Cryptography.DataProtectionScope]::CurrentUser
        )
        [Convert]::ToBase64String($protectedBytes) | Set-Content -LiteralPath $Path -NoNewline -Encoding ascii
    }
    finally {
        [Array]::Clear($plainBytes, 0, $plainBytes.Length)
    }
}

function Get-GrokSearchCredential {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string]$Path)

    if (-not (Test-Path -LiteralPath $Path)) {
        throw "Grok credential not found at $Path"
    }
    $protectedBytes = [Convert]::FromBase64String((Get-Content -LiteralPath $Path -Raw).Trim())
    $plainBytes = [System.Security.Cryptography.ProtectedData]::Unprotect(
        $protectedBytes,
        $null,
        [System.Security.Cryptography.DataProtectionScope]::CurrentUser
    )
    try {
        [Text.Encoding]::UTF8.GetString($plainBytes)
    }
    finally {
        [Array]::Clear($plainBytes, 0, $plainBytes.Length)
    }
}

function Get-GrokSearchCitations {
    [CmdletBinding()]
    param([AllowEmptyString()][string]$Text)

    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $result = [Collections.Generic.List[string]]::new()
    foreach ($match in [regex]::Matches($Text, '\[\[\d+\]\]\((https?://[^\s\)]+)\)')) {
        $url = $match.Groups[1].Value
        if ($seen.Add($url)) { $result.Add($url) }
    }
    @($result)
}

function ConvertFrom-GrokSearchResponse {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]$Response,
        [long]$ElapsedMilliseconds = 0
    )

    $texts = [Collections.Generic.List[string]]::new()
    $citationSet = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
    $citations = [Collections.Generic.List[string]]::new()
    foreach ($item in @($Response.output)) {
        foreach ($content in @($item.content)) {
            if ($content.type -eq 'output_text' -and $null -ne $content.text) {
                $texts.Add([string]$content.text)
                foreach ($url in @(Get-GrokSearchCitations -Text ([string]$content.text))) {
                    if ($citationSet.Add($url)) { $citations.Add($url) }
                }
                foreach ($annotation in @($content.annotations)) {
                    if ($annotation.url -and $citationSet.Add([string]$annotation.url)) {
                        $citations.Add([string]$annotation.url)
                    }
                }
            }
        }
    }
    foreach ($url in @($Response.citations)) {
        if ($url -and $citationSet.Add([string]$url)) { $citations.Add([string]$url) }
    }

    $usageDetails = $Response.usage.server_side_tool_usage_details
    $toolUsage = [ordered]@{
        web_search_calls = if ($null -ne $usageDetails.web_search_calls) { [int]$usageDetails.web_search_calls } else { 0 }
        x_search_calls = if ($null -ne $usageDetails.x_search_calls) { [int]$usageDetails.x_search_calls } else { 0 }
    }

    [pscustomobject][ordered]@{
        ok = $Response.status -eq 'completed'
        id = $Response.id
        model = $Response.model
        status = $Response.status
        answer = $texts -join "`n"
        citations = @($citations)
        tool_usage = [pscustomobject]$toolUsage
        elapsed_ms = $ElapsedMilliseconds
        usage = $Response.usage
    }
}

function Get-GrokSearchWarnings {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]$Result,
        [string[]]$AllowedXHandles,
        [string]$FromDate,
        [string]$ToDate
    )

    $warnings = [Collections.Generic.List[string]]::new()
    $toolCount = [int]$Result.tool_usage.web_search_calls + [int]$Result.tool_usage.x_search_calls
    if ($toolCount -gt 10) {
        $warnings.Add("High server-side tool usage: $toolCount calls. Treat broad follow-up searches as potentially expensive.")
    }

    if ($FromDate -or $ToDate) {
        $minDate = if ($FromDate) { [datetime]::ParseExact($FromDate, 'yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture) } else { [datetime]::MinValue }
        $maxDate = if ($ToDate) { [datetime]::ParseExact($ToDate, 'yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture) } else { [datetime]::MaxValue }
        foreach ($match in [regex]::Matches([string]$Result.answer, '(?<!\d)(20\d{2}-\d{2}-\d{2})(?!\d)')) {
            $answerDate = [datetime]::ParseExact($match.Groups[1].Value, 'yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture)
            if ($answerDate -lt $minDate -or $answerDate -gt $maxDate) {
                $warnings.Add("Returned date $($match.Groups[1].Value) is outside the requested range; verify the cited source directly.")
                break
            }
        }
    }

    if ($AllowedXHandles) {
        $allowed = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($handle in $AllowedXHandles) { [void]$allowed.Add($handle.TrimStart('@')) }
        foreach ($match in [regex]::Matches([string]$Result.answer, 'https?://(?:www\.)?x\.com/([^/\s\)]+)/status/')) {
            $returnedHandle = $match.Groups[1].Value
            if (-not $allowed.Contains($returnedHandle)) {
                $warnings.Add("Returned X handle @$returnedHandle is outside the allowed handle filter; verify the cited source directly.")
                break
            }
        }
    }
    @($warnings)
}

function Invoke-GrokSearch {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string]$Query,
        [ValidateSet('auto', 'web', 'x', 'both')][string]$Mode = 'auto',
        [Parameter(Mandatory)][string]$BaseUrl,
        [string]$Model = 'grok-4.5',
        [Parameter(Mandatory)][string]$CredentialPath,
        [string[]]$AllowedDomains,
        [string[]]$ExcludedDomains,
        [string[]]$AllowedXHandles,
        [string[]]$ExcludedXHandles,
        [string]$FromDate,
        [string]$ToDate,
        [ValidateRange(10, 600)][int]$TimeoutSec = 180,
        [ValidateRange(0, 2)][int]$MaxRetries = 1,
        [ValidateRange(1, 10)][int]$MaxTurns = 2,
        [ValidateSet('low', 'medium', 'high')][string]$ReasoningEffort = 'medium'
    )

    $requestParams = @{
        Query = $Query
        Mode = $Mode
        Model = $Model
        MaxTurns = $MaxTurns
        ReasoningEffort = $ReasoningEffort
    }
    foreach ($name in @('AllowedDomains', 'ExcludedDomains', 'AllowedXHandles', 'ExcludedXHandles', 'FromDate', 'ToDate')) {
        if ($PSBoundParameters.ContainsKey($name)) { $requestParams[$name] = $PSBoundParameters[$name] }
    }
    $request = New-GrokSearchRequest @requestParams
    $apiKey = Get-GrokSearchCredential -Path $CredentialPath
    $headers = @{ Authorization = "Bearer $apiKey"; 'Content-Type' = 'application/json' }
    $uri = "$($BaseUrl.TrimEnd('/'))/v1/responses"
    $body = $request | ConvertTo-Json -Depth 12 -Compress

    try {
        for ($attempt = 0; $attempt -le $MaxRetries; $attempt++) {
            $stopwatch = [Diagnostics.Stopwatch]::StartNew()
            try {
                $response = Invoke-RestMethod -Uri $uri -Method Post -Headers $headers -Body $body -TimeoutSec $TimeoutSec -MaximumRedirection 0
                $stopwatch.Stop()
                $result = ConvertFrom-GrokSearchResponse -Response $response -ElapsedMilliseconds $stopwatch.ElapsedMilliseconds
                $warnings = Get-GrokSearchWarnings -Result $result -AllowedXHandles $AllowedXHandles -FromDate $FromDate -ToDate $ToDate
                $result | Add-Member -NotePropertyName warnings -NotePropertyValue @($warnings)
                return $result
            }
            catch {
                $stopwatch.Stop()
                $status = 0
                try { $status = [int]$_.Exception.Response.StatusCode } catch {}
                $transient = $status -eq 429 -or ($status -ge 500 -and $status -le 599)
                if (-not $transient -or $attempt -ge $MaxRetries) { throw }
                Start-Sleep -Seconds ([math]::Pow(2, $attempt + 1))
            }
        }
    }
    finally {
        $headers.Authorization = $null
        $apiKey = $null
    }
}

Export-ModuleMember -Function @(
    'New-GrokSearchRequest',
    'Save-GrokSearchCredential',
    'Get-GrokSearchCredential',
    'Get-GrokSearchCitations',
    'ConvertFrom-GrokSearchResponse',
    'Get-GrokSearchWarnings',
    'Invoke-GrokSearch'
)
