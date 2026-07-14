# curl examples

Set environment variables first:

```bash
export NEWAPI_BASE_URL="https://api.lcming951.com/v1"
export NEWAPI_API_KEY="YOUR_NEWAPI_KEY"
```

Do not send this prompt to `/chat/completions`; image2 must use `/images/generations`.

Generate one image and save the JSON response:

```bash
curl -sS "$NEWAPI_BASE_URL/images/generations" \
  -H "Authorization: Bearer $NEWAPI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "gpt-image-2",
    "prompt": "A simple red cube on a white background",
    "size": "1024x1024",
    "response_format": "b64_json",
    "n": 1
  }' > image-response.json
```

Decode with Python:

```bash
python - <<'PY'
import base64, json
with open("image-response.json", "r", encoding="utf-8") as f:
    data = json.load(f)
b64 = data["data"][0]["b64_json"]
with open("image.png", "wb") as f:
    f.write(base64.b64decode(b64))
print("saved image.png")
PY
```

Edit an image with multipart form data:

```bash
curl -sS "$NEWAPI_BASE_URL/images/edits" \
  -H "Authorization: Bearer $NEWAPI_API_KEY" \
  -F "model=gpt-image-2" \
  -F "prompt=Replace the background with a clean white studio backdrop" \
  -F "image=@input.png" \
  -F "response_format=b64_json" \
  -F "n=1" > edit-response.json
```
