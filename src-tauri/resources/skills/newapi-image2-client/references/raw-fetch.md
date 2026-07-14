# Raw HTTP / fetch example

Use this for agents or platforms that do not expose an OpenAI Images SDK.

Do not call `/chat/completions` with `model: "gpt-image-2"`. Use the Images API URL below.

```js
const baseURL = (process.env.NEWAPI_BASE_URL ?? "https://api.lcming951.com/v1").replace(/\/$/, "");
const apiKey = process.env.NEWAPI_API_KEY;

const response = await fetch(`${baseURL}/images/generations`, {
  method: "POST",
  headers: {
    Authorization: `Bearer ${apiKey}`,
    "Content-Type": "application/json",
  },
  body: JSON.stringify({
    model: "gpt-image-2",
    prompt: "A simple red cube on a white background",
    size: "1024x1024",
    response_format: "b64_json",
    n: 1,
  }),
});

if (!response.ok) {
  throw new Error(`image request failed: ${response.status} ${await response.text()}`);
}

const data = await response.json();
const b64 = data.data?.[0]?.b64_json;
if (!b64) throw new Error("missing data[0].b64_json");

// Node.js:
await import("node:fs").then(({ writeFileSync }) => {
  writeFileSync("image.png", Buffer.from(b64, "base64"));
});
```

For browser-only code, do not embed the API key in frontend JavaScript. Put this call behind a backend.
