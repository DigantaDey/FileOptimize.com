# FileOptimize

Compress images and PDFs, turn an image into a PDF, or rasterise a PDF into images — served by a
single self-contained Rust binary with a minimal, high-contrast web UI.

**Live:** https://fileoptimize.com

```
┌────────────────┐  multipart/form-data   ┌────────────────────┐   ┌───────────────────────────┐   spawn   ┌──────────────┐
│  Browser UI    │ ─────────────────────▶ │  Axum routes       │   │  Engine (src/engine.rs)   │ ────────▶ │  ghostscript   │
│  (web/)        │ ◀───────────────────── │  stream · sniff    │◀─▶│  limits · memory · scratch│ ◀──────── │  poppler       │
└────────────────┘  streamed file reply   └────────────────────┘   └───────────────────────────┘           └──────────────┘
                                                    ▲
                          another app / queue worker┴  same engine, no HTTP:  fileoptimize::engine::Engine
```

The split matters: `routes` is transport (forms, headers, status codes, API keys) and holds **no**
processing policy. Every bound is enforced in `Engine::run`, so an HTTP client, a legacy client and
an embedded caller get byte-for-byte identical behaviour — which is what makes the crate safe to
expose to other applications.

## Why Rust

The previous implementation was a Flask app that buffered every upload in memory, shelled out to
Python imaging libraries, and deleted its temp files from detached threads. The rewrite keeps the
same public contract (`file` + `action` + `quality`, file download back) and fixes the structural
problems:

| Concern | Before (Flask) | After (Rust) |
| --- | --- | --- |
| Upload handling | buffered whole request in RAM | streamed to disk chunk-by-chunk, cap enforced while writing |
| Peak memory | ∝ request size × concurrent users | bounded: `MAX_CONCURRENT × per-job ceiling`, independent of file size |
| Temp files | `threading.Timer` per file, leaked on error | RAII work dir per request + TTL sweeper (error-safe) |
| Type safety | trusted the filename extension | magic-byte sniffing; mislabelled uploads are refused |
| Image bombs | none | pixel budget checked *before* decode |
| Tool failures | traceback / empty 500 | typed errors → `400/413/415/500/503` with a JSON reason |
| Shutdown | n/a | graceful drain on SIGINT/SIGTERM |
| Concurrency | WSGI workers | one async reactor + a bounded blocking pool |
| Peak memory under load | unbounded per worker | second, weighted semaphore: `MEMORY_BUDGET_MB` across all jobs |
| Reuse | HTTP only, no contract | embeddable `Engine` + versioned `/api/v1/*` + `openapi.json` |

## Memory model

Three deliberately boring techniques do all the work:

1. **Streaming in.** The multipart body is copied to disk in chunks. The request body is never
   materialised in memory, and `MAX_UPLOAD_SIZE_MB` is enforced *during* the copy, so an oversized
   upload is cut off at the byte it exceeds the cap rather than after a full read.
2. **Two bounded gates.** `MAX_CONCURRENT` (a plain semaphore) caps how many transforms run at
   once; `MEMORY_BUDGET_MB` (a semaphore weighted in MiB) caps how much working memory those
   transforms may hold *between them*. Before a decode, the job's peak is estimated from the
   image header — `width × height × 8 B` for the RGBA and RGB8 buffers, plus the encoded file —
   and that many MiB must be reserved or the job is refused with `413`. So 4000×4000 photos do not
   each get to allocate 128 MB on the way to an OOM: the budget is what stops them, not luck.
   Under load requests queue instead of failing, and CPU-heavy codec work runs on
   `spawn_blocking` so the async reactor keeps streaming other connections.
3. **Streaming out.** PDF compression and PDF→image never re-enter the heap: ghostscript/pdftoppm
   write into the request's work directory and the response is streamed from that file in 128 KiB
   slices. A 200 MB PDF costs the same resident memory as a 2 MB one.

Image decoding is the only step that genuinely needs the bytes in RAM, so it is guarded twice: the
declared dimensions are read from the header and rejected above `MAX_IMAGE_PIXELS` *before* any
pixel buffer is allocated, and `into_rgb8()` is used instead of `to_rgb8()` so the decode buffer is
consumed rather than copied.

Small results (compressed images, generated PDFs) are intentionally kept in memory — they are
bounded by the upload cap, and it lets the working directory be reclaimed the instant the response
is built.

## Endpoints

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/` | the UI (also served at `/index.html`) |
| `GET` | `/assets/style.css`, `/assets/script.js` | UI assets, embedded in the binary |
| `POST` | `/api/process` | run a transform (multipart form) |
| `POST` | `/upload` | legacy alias for `/api/process` |
| `GET` | `/api/health` | liveness probe (reports `running_jobs`) |
| `GET` | `/api/info` | effective limits + which PDF tools are present |
| `POST` | `/api/v1/process` | the machine-facing transform endpoint (API key, `store=1`) |
| `GET`/`DELETE` | `/api/v1/results/{id}` | fetch or drop a retained result |
| `GET` | `/api/v1/health`, `/api/v1/info`, `/api/v1/openapi.json` | versioned mirrors of the meta endpoints |
| `GET` | `/api/openapi.json` | the OpenAPI 3.0 document of *this build* |
| any | `*` | JSON `404` |

`POST /api/process` fields:

| Field | Type | Notes |
| --- | --- | --- |
| `file` | binary | required, non-empty |
| `action` | `compress` \| `image_to_pdf` \| `pdf_to_image` | default `compress`; hyphenated spellings accepted |
| `quality` | `1`–`100` | optional, drives `compress` only (default `DEFAULT_QUALITY`) |
| `store` | `1`/`true` | optional; retain the result and (on `/api/v1/process`) answer with JSON |

Responses are file downloads. In addition to `Content-Disposition`, the service returns:

```
Content-Type:        image/jpeg | image/png | application/pdf | image/png | application/zip
X-Original-Size:     bytes in
X-Output-Size:       bytes out
X-Original-Filename: sanitised upload name
```

Download names are derived from the sanitised upload name, per action:

| Action | Input | Output name |
| --- | --- | --- |
| `compress` | image | `photo.jpg` → `photo_optimized.jpg` (a PNG at quality ≥ 90 stays `photo_optimized.png`) |
| `compress` | PDF | `report.pdf` → `report_compressed.pdf` |
| `image_to_pdf` | image | `scan.png` → `scan.pdf` |
| `pdf_to_image` | PDF, one page | `doc.pdf` → `doc.png` |
| `pdf_to_image` | PDF, several pages | `doc.pdf` → `doc_images.zip`, entries `page_0001.png` … |

Errors are JSON: `{ "error": "quality 101 out of range: must be between 1 and 100" }`.

## Using it from another application

[`docs/API.md`](docs/API.md) is the full integration guide. The short version:

- **Programs** should call `POST /api/v1/process` and read `GET /api/v1/info` first (limits,
  accepted actions, whether ghostscript/poppler exist here, whether a key is required).
- **Callers that cannot stream a download** send `store=1` and get a JSON descriptor with an
  `id`; the result is then `GET`/`DELETE`-able at `/api/v1/results/{id}` until the TTL sweeper
  collects it. Retained payloads live in `TMP_DIR/results/`, expire like everything else, and
  cannot be listed — holding the id *is* the capability.
- **Errors are machine-readable**: `{"error": "...", "code": "payload_too_large"}`, with stable
  codes for `400/401/404/413/415/500/503`.
- **Rust callers** skip the transport entirely:

```rust
use fileoptimize::engine::{Action, Engine, Input, Job};

let engine = Engine::from_env();
let outcome = engine
    .run(Job::new(Action::Compress, "holiday.jpg", Input::Bytes(bytes)).with_quality(70))
    .await?;
let payload = outcome.output.read_all()?;   // in memory or on disk, the caller's choice
```

The crate exposes `engine`, `config`, `error`, `processing`, `detect` and `sanitize`, so a
downstream service can reuse the sniffing and the filename hardening without taking the HTTP
dependency at all.

## Limits, checks and bounds

Everything below is configurable and every limit produces a *specific* status code and message —
never a generic 500.

| Env var | Default | Enforced as |
| --- | --- | --- |
| `HOST` | `0.0.0.0` | bind address |
| `PORT` | `8080` | bind port |
| `TMP_DIR` | `./tmp` | scratch root (created at boot) |
| `MAX_UPLOAD_SIZE_MB` | `50` (1–500) | `413` while streaming the upload |
| `MAX_IMAGE_PIXELS` | `25000000` | `400` before decoding (`1e6`–`1e8`) |
| `MAX_CONCURRENT` | `4` (1–64) | queue depth |
| `MEMORY_BUDGET_MB` | `1024` (16–65000) | `413` when one job alone would exceed it |
| `API_KEYS` | *unset* | `401` on `/api/v1/*` when set |
| `CLEANUP_TTL_SECS` | `1800` (60–86400) | temp-file lifetime |
| `DEFAULT_QUALITY` | `60` (1–100) | used when `quality` is absent |
| `PDF_RASTER_DPI` | `150` (36–600) | pdftoppm resolution |
| `TOOL_TIMEOUT_SECS` | `120` (5–1800) | `500` + kill for a wedged tool |
| `RUST_LOG` | `info` | tracing filter |

Input validation:

- **Magic bytes, not extensions.** A PDF named `holiday.jpg` is treated as a PDF. Unknown types → `415`.
- **Action/type matrix is checked**, so `pdf_to_image` on a PNG and `image_to_pdf` on a PDF are both
  rejected with an explanation instead of producing junk.
- **Empty uploads** are rejected; **0-dimension or absurd-dimension images** are rejected.
- **Filenames are sanitised** before they are echoed back: path components, `..`, quotes, control
  characters and non-ASCII are removed, and the name is truncated — header injection is not possible.
- **Storage names are UUIDs** inside a per-request directory, so user input never influences a path.
- **Quality is strictly bounded** to `1..=100`; `0`, `101`, `50.5`, `1e2` and non-numeric input fail.
- **Memory is reserved before it is used**: a decode that cannot fit `MEMORY_BUDGET_MB` is refused
  from the header dimensions, without touching a pixel buffer.
- **Caller files are never mutated.** `Input::Path` is opened read-only; only the engine's own UUID
  scratch directory is ever deleted.
- **Result ids are format-checked** (exactly 32 lowercase hex characters) before they are used as a
  directory name, so `..`, separators and case tricks cannot escape the results store.
- **Malformed input cannot panic**: the JPEG marker walker (`detect::jpeg_is_cmyk`) tolerates
  truncation and never trusts a declared segment length beyond the buffer.
- **Missing tools degrade visibly**: `/api/info` reports `ghostscript`/`poppler` availability, and a
  request needing an absent tool returns `503` with the tool named.
- **No deadlocks on chatty children**: tool `stderr` is drained concurrently and each run is capped
  by `TOOL_TIMEOUT_SECS`.

## Actions in detail

- **`compress` (image)** — decode, then re-encode. JPEG stays JPEG at the requested quality; PNG
  stays PNG only at `quality ≥ 90` (PNG has no quality knob, so anything lower is strictly better as
  JPEG); WebP/GIF/BMP/TIFF are normalised to JPEG. Alpha is flattened, matching the old behaviour.
- **`compress` (PDF)** — ghostscript `pdfwrite` with the same `/PDFSETTINGS` mapping the original
  used (`≤25` screen, `≤50` ebook, `≤75` printer, else prepress) plus `-dSAFER`, so text stays text
  and the file is re-compressed on the fly rather than rasterised.
- **`image_to_pdf`** — a baseline JPEG is embedded *verbatim* as a `/DCTDecode` XObject (no
  transcode, no second lossy generation, no pixel buffer at all); anything else is normalised to RGB
  JPEG first. The one-page PDF is written by a purpose-built writer with a hand-computed xref table
  and a DPI-correct `/MediaBox`.
- **`pdf_to_image`** — pdftoppm renders pages into the work directory. One page → that PNG; several
  pages → a streamed ZIP (`page_0001.png`, …) assembled entry-by-entry from disk.

## Development

```bash
cargo run                          # http://127.0.0.1:8080
cargo test                         # unit + HTTP integration tests
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

The toolchain file pins `stable` with `rustfmt` and `clippy`. `ghostscript` and `poppler-utils` are
needed only for the PDF actions; without them the rest of the service works and those two actions
return `503`.

Tests cover the pure logic (sanitisation, sniffing, bounds, the PDF writer's xref offsets, the ZIP
writer, the sweeper) and the full HTTP surface through the real router: `400/404/413/415/500/503`
paths, header contents, `Content-Disposition` sanitising, quality clamping, pixel-budget refusal,
byte-exact JPEG embedding, concurrency through the semaphore, and — the point of the RAII guard —
that **no working directory is left behind** on success or rejection. PDF actions run against real
ghostscript/poppler when they are installed and assert the documented `503` when they are not.

## Deployment

```bash
docker build -t fileoptimize .
docker run -d --name fileoptimize -p 8080:8080 \
  -e MAX_UPLOAD_SIZE_MB=50 -e MAX_CONCURRENT=4 \
  -e MEMORY_BUDGET_MB=2048 \
  -e API_KEYS="$(openssl rand -hex 24)" \
  fileoptimize
```

`MEMORY_BUDGET_MB` is the knob to match to the container's memory limit; `API_KEYS` turns on
key auth for `/api/v1/*` (leave it unset to serve anyone, which is what a public demo wants).

Or behind any reverse proxy:

```bash
cargo build --release
PORT=8080 RUST_LOG=info ./target/release/fileoptimize
```

The binary embeds the UI, so deployment is one file plus two external tools. Point your proxy at
`0.0.0.0:8080`, keep `multipart` bodies enabled, and set `client_max_body_size` (nginx) or
equivalent to at least `MAX_UPLOAD_SIZE_MB` so the *service* — not the proxy — is what reports an
oversized upload.

## CI

`.github/workflows/ci.yml` runs format → clippy (`-D warnings`) → tests (Linux **and** macOS) →
release build → a smoke test that boots the actual release binary and exercises `/api/health`,
`/api/info`, the UI, and the error paths → a Docker image build with its own container smoke test.

## Repository layout

```
src/
  main.rs            boot, tracing, graceful shutdown
  lib.rs             router + middleware (tracing, CORS, selective gzip)
  config.rs          env-driven limits, clamped
  state.rs           AppState (config + engine handle) + RAII WorkDir guard
  engine.rs          Engine, Job, JobId, JobOutcome — all policy lives here
  routes.rs          HTTP transport: forms, headers, status codes, API keys
  detect.rs          magic-byte sniffing, YCCK JPEG probe
  sanitize.rs        filename/extension hardening
  cleanup.rs         TTL sweeper for streamed artifacts
  error.rs           AppError → status + JSON
  processing/
    mod.rs           Output, decode bounds, quality/format selection
    image.rs         JPEG/PNG encoders
    pdf.rs           ghostscript + pdftoppm wrappers (timeouts, stderr)
    pdf_gen.rs       dependency-free one-page PDF writer
    zip.rs           streaming archive writer
web/                 index.html · style.css · script.js · openapi.json   (embedded via include_str!)
docs/API.md          integration guide for other applications
tests/               unit.rs · api.rs · engine.rs
```

## Migration notes

- `POST /upload` still accepts the original field names, so existing clients keep working; new
  clients should use `POST /api/process`, and programs should prefer `POST /api/v1/process`.
- Error bodies gained a `code` field alongside `error`. Anything that only read `error` is
  unaffected.
- Errors that used to surface as an HTML traceback now return a status code and a one-line JSON
  `error` — including `503` when ghostscript or poppler is missing.
- `quality` now accepts `1..=100` (previously `10..=100` in the UI).
- Multi-page `pdf_to_image` output entries are named `page_0001.png` (zero-padded) instead of
  `page-1.png`.
- The former Flask implementation (`app.py`, `templates/`, `static/`, `requirements.txt`) is
  removed; it remains in git history.
