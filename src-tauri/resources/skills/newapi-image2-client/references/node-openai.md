# Node OpenAI SDK example

Install if needed:

```bash
npm install openai
```

Generate and save one image:

```js
import fs from "node:fs";
import OpenAI from "openai";

const client = new OpenAI({
  apiKey: process.env.NEWAPI_API_KEY,
  baseURL: (process.env.NEWAPI_BASE_URL ?? "https://api.lcming951.com/v1").replace(/\/$/, ""),
});

const result = await client.images.generate({
  model: "gpt-image-2",
  prompt: "A simple red cube on a white background",
  size: "1024x1024",
  response_format: "b64_json",
  n: 1,
});

const imageB64 = result.data[0].b64_json;
fs.writeFileSync("image.png", Buffer.from(imageB64, "base64"));
console.log("saved image.png");
```

Notes:

- `NEWAPI_BASE_URL` should include `/v1`; default to `https://api.lcming951.com/v1`.
- Use `client.images.generate(...)`; do not use `client.chat.completions.create(...)` with `model: "gpt-image-2"`.
- Use raw `fetch` if the platform's SDK wrapper does not support Images API.
