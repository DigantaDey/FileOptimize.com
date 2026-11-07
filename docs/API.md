# FileOptimize API — integration guide

This document is for **another application** calling FileOptimize: a web app that uploads
on behalf of a user, a queue worker that tidies attachments, or a Rust service that wants
the transform without a network hop.

Three equivalent surfaces, one engine behind all of them:

| Surface | When to use it |
| --- | --- |
| `POST /api/v1/process` | **Default for programs.** Versioned, optional API key, JSON descriptor, retainable results. |
| `POST /api/process`, `POST /upload` | Unauthenticated compatibility surface — the bundled UI and clients written for the original Python service. |
| `fileoptimize::engine::Engine` (Rust crate) | Embedding: no socket, no multipart, same validation and the same memory accounting. |

Because every surface calls the same [`Engine::run`], there is no "stricter HTTP path" and
no "looser library path": a file accepted by one is accepted by all.

[`Engine::run`]: ../src/engine.rs

---

## 1. Discover what this deployment allows first

```bash
curl -s $HOST/api/v1/info | jq
```

```json
{
  "service": "fileoptimize",
  "version": "1.0.0",
  "actions": ["compress", "image_to_pdf", "pdf_to_image"],
  "quality_range": [1, 100],
  "max_upload_size": 52428800,
  "max_image_pixels": 25000000,
  "max_concurrent": 4,
  "default_quality": 60,
  "pdf_raster_dpi": 150,
  "tool_timeout_secs": 120,
  "memory_budget_mb": 1024,
  "memory_available_mb": 1024,
  "result_ttl_secs": 1800,
  "auth_required": false,
  "tools": { "ghostscript": true, "poppler": true }
}
```

Read `tools` before offering PDF actions to your users — a host without ghostscript or
poppler answers `503 external_tool_missing`, and you would rather not discover that in the
middle of a batch. `auth_required` tells you whether to attach a key.

`GET /api/health` (also `/api/v1/health`) answers `{status:"ok", running_jobs:N}`;
`running_jobs` is useful as a saturation signal for autoscaling.

Both meta endpoints stay open even when `API_KEYS` is set, so a load balancer never needs
a secret.

## 2. Authentication

The v1 surface is open by default. Set `API_KEYS` (comma- or space-separated) to require a
key, then send any one of:

```
x-api-key: <key>
Authorization: Bearer <key>
?api_key=<key>          # for clients that cannot set headers; avoid if you can
```

- A missing or unknown key is `401 unauthorized` plus `WWW-Authenticate: Bearer …`.
- Keys are compared as exact strings; generate long random ones (`openssl rand -hex 32`).
- The key gates `/api/v1/process` and the results endpoints only. `/api/health`,
  `/api/info`, `/api/v1/health`, `/api/v1/info` and the unversioned routes are public by
  design; if you need them locked too, put a gateway in front.
- One key authorises everything it can reach. There are no scopes and no per-key quotas —
  for that, run one instance per tenant or authenticate at the gateway.

## 3. Convert a file

`multipart/form-data`, one file per request:

| Field | Required | Notes |
| --- | --- | --- |
| `file` | yes | non-empty, at most `max_upload_size`. The type is sniffed from magic bytes — the filename and the part's `Content-Type` are never trusted. |
| `action` | no | `compress` (default), `image_to_pdf`, `pdf_to_image`. `image-to-pdf` / `pdf-to-image` are also accepted. |
| `quality` | no | integer `1`–`100`, used by `compress`. Absent or empty → `default_quality`. |
| `store` | no | `1`/`true`/`yes`/`on` → retain the result and reply with JSON (v1) instead of the bytes. |

Unknown fields are ignored rather than rejected, so a stray browser form field cannot fail
an otherwise valid job.

```bash
# compress a photo to JPEG quality 62
curl -s -o holiday_optimized.jpg \
  -F "file=@holiday.jpg" \
  -F "action=compress" \
  -F "quality=62" \
  -H "x-api-key: $KEY" \
  $HOST/api/v1/process

# one image → one-page PDF
curl -s -o receipt.pdf -F "file=@receipt.png" -F "action=image_to_pdf" $HOST/api/v1/process

# PDF → images (ZIP of PNGs when it has several pages)
curl -s -o pages.zip -F "file=@report.pdf" -F "action=pdf_to_image" $HOST/api/v1/process
```

### Response headers worth reading

```
Content-Disposition: attachment; filename="holiday_optimized.jpg"
Content-Type: image/jpeg
X-Original-Size: 3811204
X-Output-Size: 481902
X-Original-Filename: holiday.jpg
```

They are listed in `Access-Control-Expose-Headers`, so a browser `fetch` can read them
directly. `X-Original-Filename` is the sanitised client name (last path component only, no
control characters, quotes or `..`), which makes it safe to render.

Download names follow the action: `photo.jpg` → `photo_optimized.jpg` (image compress),
`report.pdf` → `report_compressed.pdf` (PDF compress), `scan.png` → `scan.pdf`
(image→PDF), `doc.pdf` → `doc.png` or `doc_images.zip` (PDF→image, entries `page_0001.png`).

`compress` never hands back more bytes than you sent. A file that is already smaller than any
encode of it — a 24×16 PNG costs fewer bits than the ~700-byte JPEG of the same pixels — comes
back as its own pixels with its own media type, so `X-Output-Size` equals `X-Original-Size`
(and `saved_bytes: 0` in a retained descriptor). Read the returned `Content-Type` and
`Content-Disposition` rather than assuming `_optimized.jpg`.

### JavaScript

```js
const form = new FormData();
form.append("file", fileInput.files[0]);
form.append("action", "compress");
form.append("quality", "70");

const res = await fetch(`${apiBase}/api/v1/process`, {
  method: "POST",
  body: form,
  headers: key ? { "x-api-key": key } : {},
});
if (!res.ok) {
  const { error, code } = await res.json();
  throw new Error(`${code}: ${error}`);
}
const blob = await res.blob();
const before = Number(res.headers.get("x-original-size")) || blob.size;
const after = Number(res.headers.get("x-output-size")) || blob.size;
console.log(`saved ${Math.round((1 - after / before) * 100)}%`);
```

Send the `FormData` without setting `Content-Type` yourself — the browser has to add the
multipart boundary.

### Python

```python
import requests

with open("holiday.jpg", "rb") as fh:
    r = requests.post(
        f"{host}/api/v1/process",
        headers={"x-api-key": key} if key else {},
        files={"file": ("holiday.jpg", fh, "image/jpeg")},
        data={"action": "compress", "quality": "70", "store": "1"},
        timeout=120,
        stream=True,
    )
r.raise_for_status()
job = r.json()                      # store=1 → a descriptor, not the file
print(job["saved_bytes"], job["download_path"])
out = requests.get(f"{host}{job['download_path']}", headers={"x-api-key": key}, timeout=120)
open(job["filename"], "wb").write(out.content)
requests.delete(f"{host}{job['download_path']}", headers={"x-api-key": key}, timeout=30)
```

## 4. Retained results (`store=1`)

Some callers cannot consume a streamed body — a queue consumer, a server-side renderer, a
mobile client on a lossy link. Ask for retention and the reply is a descriptor:

```json
{
  "ok": true,
  "id": "0d1f5c8e9a2b4f6d8e2a1c9b7d3e5f44",
  "action": "compress",
  "quality": 70,
  "original_name": "holiday.jpg",
  "original_size": 3811204,
  "filename": "holiday_optimized.jpg",
  "content_type": "image/jpeg",
  "output_size": 512334,
  "saved_bytes": 3298870,
  "download_path": "/api/v1/results/0d1f5c8e9a2b4f6d8e2a1c9b7d3e5f44",
  "max_age_secs": 1800
}
```

- `download_path` is **relative** on purpose: put your own origin in front of it and the
  link keeps working behind a proxy, a path prefix, or `file://`.
- `GET /api/v1/results/{id}` streams the stored bytes with the same headers as a direct
  download; `DELETE` removes them early and answers `204` (`404` if already gone).
- A result is a **capability**: whoever holds the id can fetch it, and there is deliberately
  no "list my results" endpoint — the server keeps no per-key ownership model, so listing
  would mean leaking other callers' ids. Treat the id like a signed URL.
- Retention is bounded: entries live under `TMP_DIR/results/<id>/` and a background sweeper
  collects anything older than `result_ttl_secs`. Nothing is kept forever, and a restart
  does not resurrect an expired result — expect `404 not_found` after the window.
- Ids are exactly 32 lowercase hex characters. Anything else is a `400` before the
  filesystem is touched, which is what makes an id safe to interpolate into a path.
- On `/api/process` (and `/upload`) `store=1` still returns the file itself; retention is
  the extra, not a substitution.

## 5. Errors

Every failure is JSON, on every route, with the same envelope:

```json
{ "error": "quality 101 out of range: must be between 1 and 100", "code": "bad_request" }
```

| Status | `code` | Meaning | Client action |
| --- | --- | --- | --- |
| `400` | `bad_request` | No `file` part, empty file, unknown `action`, `quality` outside 1–100, image beyond `max_image_pixels`, malformed multipart, bad result id. | Fix the request. Not retryable as-is. |
| `401` | `unauthorized` | No key, or a key that is not configured. | Attach a valid key. |
| `404` | `not_found` | Unknown route, or a result that never existed or has been swept. | Re-run the job. |
| `413` | `payload_too_large` | The upload is over `max_upload_size`, **or** the transform would need more working memory than `memory_budget_mb`. | Downscale/re-encode first; the message names the limit. |
| `415` | `unsupported_media_type` | The bytes are not a type this action can use (judged from magic bytes, not the extension). | Pick another action or convert first. |
| `503` | `external_tool_missing` | This host lacks ghostscript/poppler for a PDF action. | Do not retry until the operator installs them; check `/api/v1/info`. |
| `500` | `internal_error` | Decoder, encoder or tool failed on otherwise-valid input. | Retry once, then report. The scratch directory is still cleaned up. |

Status and code are both stable; branch on `code`, since some proxies rewrite or hide
`413`/`503` bodies but never rewrite JSON content.

A mid-stream over-cap upload is answered with `413` while the request body is still
arriving — you do not have to send the whole file to learn it is too big.

## 6. Throughput, backpressure and retries

- The server runs at most `max_concurrent` transforms at once and reserves
  `memory_budget_mb` of working memory across them. Extra requests are **queued, not
  refused**: latency grows instead of returning `429`.
- Size your client concurrency to `max_concurrent` (or to
  `memory_budget_mb ÷ your typical per-job footprint`, whichever is smaller) if you want
  predictable latency. A 24 MP photo peak-reserves ~200 MB.
- Long uploads: the read timeout matters more than the connect timeout — a PDF rasterising
  at 600 DPI can legitimately take `tool_timeout_secs`.
- Retries are safe. The service is stateless apart from retained results: no caching, no
  session, nothing that a duplicate request would corrupt. Re-send with a fresh
  `store=1` and you get a fresh id.
- There is no per-tenant rate limit or quota in the service; enforce that at the gateway.

## 7. Embedding from Rust

`Cargo.toml`:

```toml
fileoptimize = { git = "https://github.com/DigantaDey/FileOptimize.com", branch = "main" }
```

```rust
use fileoptimize::engine::{Action, Engine, Input, Job};

let engine = Engine::from_env();          // same defaults, same clamps as the server

// From memory (a queue payload, an in-process buffer)…
let outcome = engine
    .run(Job::new(Action::Compress, "holiday.jpg", Input::Bytes(bytes)).with_quality(70))
    .await?;
let payload: Vec<u8> = outcome.output.read_all()?;

// …or from a path the caller owns. The engine never moves or deletes it, and its own
// scratch directory is removed on drop — including when the transform fails.
let outcome = engine
    .run(Job::new(Action::ImageToPdf, "scan.png", Input::Path(path)))
    .await?;
```

`JobOutcome` carries everything a job record needs: sanitised `original_name`,
`original_size`, the output's `filename`/`content_type`/`size()`, the resolved `quality`,
and `bytes_saved()`. `Output::is_streamed()` tells you whether the payload is a file that
must outlive the call — for that case the engine hands its directory to the TTL sweeper
instead of deleting it, so a caller that returns early still cannot leak disk.

Use `Job::stored()` when the transform feeds a later request (a response you must
re-serialise, a webhook retry, a preview that is fetched by id) — the bytes move into the
results store under `TMP_DIR/results/<id>/`, and `engine.result(&id)` / `engine.discard(&id)`
are the same operations the HTTP endpoints perform.

## 8. Contract and versioning

- `/api/v1/*` is versioned. Additive changes (new fields, new endpoints) ship without a new
  version; anything that removes or redefines a field, status code or `code` value requires
  `/api/v2` and a deprecation window on v1.
- The unversioned `/api/process` and `/upload` are frozen compatibility aliases: they will
  not gain fields or change semantics, and they will not be removed.
- This document, the routes and `web/openapi.json` are one file set in one repository, and
  `GET /api/openapi.json` serves the spec **of the running build** — generate a client from
  it rather than from a copy:

```bash
npx @openapitools/openapi-generator-cli generate \
  -i $HOST/api/openapi.json -g rust -o ./fileoptimize-client
```

Verify against a live instance with `GET /api/v1/health` and one `compress` round trip; the
`tools` block decides whether the PDF paths are exercised too.
