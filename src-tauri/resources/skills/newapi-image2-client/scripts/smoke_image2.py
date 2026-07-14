#!/usr/bin/env python3
"""Smoke test gpt-image-2 through a New API endpoint.

Environment:
  NEWAPI_BASE_URL=https://api.lcming951.com/v1
  NEWAPI_API_KEY=...

Usage:
  python scripts/smoke_image2.py --prompt "a red cube" --out image.png
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import sys
import urllib.error
import urllib.request


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--prompt", default="A simple red cube on a white background")
    parser.add_argument("--out", default="image.png")
    parser.add_argument("--size", default="1024x1024")
    args = parser.parse_args()

    base_url = os.environ.get("NEWAPI_BASE_URL", "https://api.lcming951.com/v1").rstrip("/")
    api_key = os.environ.get("NEWAPI_API_KEY", "")
    if not api_key:
        print("Missing NEWAPI_API_KEY", file=sys.stderr)
        return 2
    if not base_url.endswith("/v1"):
        print("NEWAPI_BASE_URL should include /v1", file=sys.stderr)
        return 2

    payload = {
        "model": "gpt-image-2",
        "prompt": args.prompt,
        "size": args.size,
        "response_format": "b64_json",
        "n": 1,
    }
    request = urllib.request.Request(
        f"{base_url}/images/generations",
        data=json.dumps(payload).encode("utf-8"),
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )

    try:
        with urllib.request.urlopen(request, timeout=180) as response:
            raw = response.read()
    except urllib.error.HTTPError as error:
        body = error.read().decode("utf-8", errors="replace")
        print(f"HTTP {error.code}: {body[:1000]}", file=sys.stderr)
        return 1

    data = json.loads(raw)
    image_b64 = data.get("data", [{}])[0].get("b64_json")
    if not image_b64:
        print("Missing data[0].b64_json", file=sys.stderr)
        print(json.dumps(data, ensure_ascii=False)[:1000], file=sys.stderr)
        return 1

    with open(args.out, "wb") as f:
        f.write(base64.b64decode(image_b64))
    print(f"saved {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
