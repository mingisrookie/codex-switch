---
name: newapi-image2-client
description: Use when an agent needs to generate or edit images through a user-provided OpenAI-compatible New API endpoint using gpt-image-2, especially when Hermes/Hermers, OpenClaw, Claude Code, WorkBunny, Codex, or other agents need exact Images API request shapes, b64_json handling, or fixes for accidentally calling chat/completions.
---

# New API Image2 Client

## Codex Switch installation

When this skill is installed by Codex Switch, use the bundled PowerShell helper first. It reads the configured URL and the current user's DPAPI-protected Key without printing either credential:

```powershell
$codexHome = if ($env:CODEX_HOME) { $env:CODEX_HOME } else { Join-Path $env:USERPROFILE '.codex' }
$image2 = Join-Path $codexHome 'skills\newapi-image2-client\scripts\image2.ps1'
& $image2 -Action generate -Prompt "<prompt>" -OutputPath "<absolute-output.png>"
```

For edits, add `-Action edit -ImagePath "<input.png>"` and optionally `-MaskPath "<mask.png>"`. Never open, copy, print, or request the DPAPI credential file. If the helper reports that configuration is missing, ask the user to configure Image2 in Codex Switch.

## Purpose

Call `gpt-image-2` through the New API endpoint, not through `api.openai.com`, unless the user explicitly gives a different endpoint.

Default endpoint:

```text
NEWAPI_BASE_URL=https://api.lcming951.com/v1
```

Use the OpenAI-compatible Images API:

- Generate: `POST {NEWAPI_BASE_URL}/images/generations`
- Edit: `POST {NEWAPI_BASE_URL}/images/edits`
- Model: `gpt-image-2`
- Response format: `b64_json`
- Initial safe default: `n: 1`

## Hard rule: image2 is not a chat model

Do **not** set `gpt-image-2` as the model for `/chat/completions`.

The correct call is:

```text
POST https://api.lcming951.com/v1/images/generations
```

If an agent UI only has a chat model picker, do not choose `gpt-image-2` there. Use a tool/script/raw HTTP call to the Images API instead.

This exact error means the wrong endpoint was used:

```text
The 'gpt-image-2' model is not supported when using Codex with a ChatGPT account.
```

Recovery: retry the same prompt through `/images/generations` with `response_format=b64_json`.

## Required inputs

Ask the user for these if missing:

1. The user's New API key, configured through Codex Switch or `NEWAPI_API_KEY` outside a Codex Switch installation.
2. The base URL, configured through Codex Switch or `NEWAPI_BASE_URL`, defaulting to `https://api.lcming951.com/v1` when absent.
3. Prompt and desired output path.

Never ask for or expose the server's internal proxyd key, account cookies, refresh tokens, or admin credentials.

## Generation workflow

1. Normalize `NEWAPI_BASE_URL`:
   - If missing, use `https://api.lcming951.com/v1`.
   - Remove trailing `/`.
   - Ensure it includes `/v1`.
2. Send JSON to `/images/generations`.
3. Set:
   - `model: "gpt-image-2"`
   - `response_format: "b64_json"`
   - `n: 1` unless the user explicitly asks for multiple images and billing supports it.
4. Decode `data[0].b64_json` to a `.png` file.
5. Return the saved file path and a short success summary.

Minimal JSON body:

```json
{
  "model": "gpt-image-2",
  "prompt": "A simple red cube on a white background",
  "size": "1024x1024",
  "response_format": "b64_json",
  "n": 1
}
```

## Editing workflow

Use multipart form data against `/images/edits`:

- `model=gpt-image-2`
- `prompt=<edit instruction>`
- `image=@input.png`
- optional `mask=@mask.png`
- `response_format=b64_json`
- `n=1`

If the platform cannot send multipart requests, use a small local script or raw HTTP client instead of trying to force chat-completions.

## Platform routing

- **OpenAI SDK compatible platforms**: configure `base_url` / `baseURL` to `https://api.lcming951.com/v1` and use the user's New API key.
- **Claude Code / Codex / terminal agents**: prefer raw `curl`, Python, or Node examples from `references/`.
- **OpenClaw / WorkBunny / Hermes-like agents**: configure a custom OpenAI-compatible provider only for the base URL/key. Do not use the chat model picker for image2. If the UI has no Images API action, call `curl`, Python, Node, or raw fetch from an agent tool.
- **Browser/frontend-only environments**: do not expose the API key in client-side JavaScript. Route through a backend.

## Important constraints

- Do not use `response_format=url`; use `b64_json`.
- Do not call `/chat/completions` for image generation, even if the provider model list shows `gpt-image-2`.
- Do not call `/responses` for this skill unless the user explicitly says their endpoint supports image tool calls there.
- Keep initial `n=1`; the service may price images per generated image.
- If `/v1/models` does not list `gpt-image-2`, tell the user the New API key or group is not yet enabled for image2.

## Troubleshooting

- `401`: New API key is invalid, expired, or missing the `Bearer` prefix.
- `404` or model not found: `gpt-image-2` is not exposed to this key/group.
- `400 response_format`: switch to `b64_json`.
- `model is not supported when using Codex with a ChatGPT account`: the request went to `/chat/completions` or `/responses` as a plain model call. Use `/images/generations`.
- Empty `data` or missing `b64_json`: report the raw error summary without exposing keys.
- Request too large or timeout: reduce image size/input count; retry with `n=1`.
- No file saved: verify the base64 decode step and output path.

## References

Read only the relevant reference:

- `references/curl.md` for shell/curl.
- `references/python-openai.md` for Python SDK.
- `references/node-openai.md` for Node SDK.
- `references/raw-fetch.md` for raw fetch/HTTP agents.

Use `scripts/smoke_image2.py` when the environment has Python and the user wants a quick local smoke test.
