# Python OpenAI SDK example

Install if needed:

```bash
pip install openai
```

Generate and save one image:

```python
import base64
import os
from openai import OpenAI

client = OpenAI(
    api_key=os.environ["NEWAPI_API_KEY"],
    base_url=os.environ.get("NEWAPI_BASE_URL", "https://api.lcming951.com/v1").rstrip("/"),
)

result = client.images.generate(
    model="gpt-image-2",
    prompt="A simple red cube on a white background",
    size="1024x1024",
    response_format="b64_json",
    n=1,
)

image_b64 = result.data[0].b64_json
with open("image.png", "wb") as f:
    f.write(base64.b64decode(image_b64))

print("saved image.png")
```

Notes:

- `NEWAPI_BASE_URL` should include `/v1`; default to `https://api.lcming951.com/v1`.
- Use `client.images.generate(...)`; do not use `client.chat.completions.create(...)` with `model="gpt-image-2"`.
- Do not set the base URL to `https://api.openai.com/v1` unless the user explicitly requests direct OpenAI.
- If the SDK version does not expose `response_format`, use the raw HTTP example instead.
