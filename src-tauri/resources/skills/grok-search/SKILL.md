---
name: grok-search
description: Use whenever the user asks to search, look up, verify, research, find current information or sources, investigate news or products, inspect X posts or sentiment, or when an answer depends on temporally unstable internet information.
---

# Grok Search

Use the configured Grok 4.5 channel as the primary internet research provider. The user only asks questions; the agent selects and runs the search mode.

## Run

Execute:

```powershell
$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
$grokSearch = Join-Path $codexHome 'skills\grok-search\scripts\grok-search.ps1'
& $grokSearch -Query "<research prompt>" -Mode <mode>
```

The URL and current-user DPAPI-protected API Key are configured through Codex Switch. Never open, copy, print, or request the credential file. If the script reports missing configuration, ask the user to configure Grok Search in Codex Switch.

Select the narrowest useful mode:

| Mode | Use for |
| --- | --- |
| `web` | Documentation, official sites, products, prices, news, general facts |
| `x` | X posts, accounts, threads, community reaction, real-time social signals |
| `both` | Deep research that benefits from authoritative Web sources plus X discussion |
| `auto` | Only when the correct source class is genuinely unclear |

Useful optional filters: `-AllowedDomains`, `-ExcludedDomains`, `-AllowedXHandles`, `-ExcludedXHandles`, `-FromDate`, and `-ToDate`. Restrict broad X searches by handle/date when possible because unconstrained X requests may be slow.

Cost control defaults to `-MaxTurns 2 -ReasoningEffort medium`. Use `-MaxTurns 1 -ReasoningEffort low` for a simple lookup, and raise turns/effort only for genuinely deep research. Prompt wording alone does not reliably cap server-side tool calls.

## Research contract

1. Put the exact task, time window, preferred source quality, and desired output structure in `-Query`.
2. For factual or high-stakes work, ask Grok to distinguish primary evidence, reporting, social claims, and inference.
3. Parse the JSON result. Treat `ok`, `tool_usage`, `citations`, `model`, and `elapsed_ms` as execution evidence.
4. Treat any `warnings` as a verification requirement. The configured third-party channel has been observed returning an X result outside requested date/handle filters; do not repeat such claims without opening the cited source or independently verifying it.
5. Open decisive cited pages separately when exact wording, a specific page, or high-stakes verification requires direct inspection.
6. Cite the returned source URLs near the supported claims in the final answer.
7. Never print, read aloud, or copy the credential file or plaintext API key.
8. If Grok fails, retry once only for transient `429`/`5xx` failures. For X timeouts, narrow the date/handle scope once. Then use the built-in Web tool as fallback and state briefly that Grok was unavailable.

Do not use search for stable, timeless facts unless the user asks for verification.
