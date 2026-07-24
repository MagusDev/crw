# Recipe: Scrape a JS-heavy SPA

**Goal:** Go from blank output to clean markdown on a React/Next.js/Vue SPA using the diagnostic loop: plain scrape → inspect `renderedWith` → add `renderJs` + `waitFor` → pin a renderer.

**Target URL used in this recipe:** `https://vercel.com/changelog` — a Next.js SPA that ships an empty `<div id="__next">` shell to plain HTTP fetchers.

---

## Step 1 — Plain scrape (baseline)

Always start without JS rendering. If the page is server-side rendered you save cost and latency.

:::tabs
::tab{title="cURL"}
```bash
curl -s -X POST https://api.fastcrw.com/v1/scrape \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://vercel.com/changelog",
    "formats": ["markdown"]
  }'
```
::tab{title="Python (crw SDK)"}
```python
from crw import CrwClient

client = CrwClient()  # reads CRW_API_KEY from env

result = client.scrape(
    "https://vercel.com/changelog",
    formats=["markdown"],
)

markdown = result.get("markdown", "")
rendered_with = result["metadata"]["renderedWith"]

print(f"renderedWith: {rendered_with}")
print(f"markdown length: {len(markdown)} chars")
print(markdown[:300] or "(empty)")
```
:::

**Expected output (SPA shell — nothing useful):**

```json
{
  "success": true,
  "data": {
    "markdown": "",
    "metadata": {
      "title": "Changelog – Vercel",
      "sourceURL": "https://vercel.com/changelog",
      "statusCode": 200,
      "renderedWith": "http",
      "elapsedMs": 180
    }
  }
}
```

`renderedWith: "http"` confirms no browser was used. The empty markdown is the SPA shell — all content is injected by JavaScript after load.

---

## Step 2 — Add `renderJs: true` and a `waitFor`

Force browser rendering and give the page time to hydrate.

:::tabs
::tab{title="cURL"}
```bash
curl -s -X POST https://api.fastcrw.com/v1/scrape \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://vercel.com/changelog",
    "formats": ["markdown"],
    "renderJs": true,
    "waitFor": 2000
  }'
```
::tab{title="Python (crw SDK)"}
```python
from crw import CrwClient

client = CrwClient()

result = client.scrape(
    "https://vercel.com/changelog",
    formats=["markdown"],
    render_js=True,
    wait_for=2000,
)

markdown = result.get("markdown", "")
rendered_with = result["metadata"]["renderedWith"]

print(f"renderedWith: {rendered_with}")
print(f"markdown length: {len(markdown)} chars")
print(markdown[:500])
```
:::

**Expected output (hydrated content):**

```json
{
  "success": true,
  "data": {
    "markdown": "# Changelog\n\n## June 2026\n\n### Build output compression...",
    "metadata": {
      "title": "Changelog – Vercel",
      "sourceURL": "https://vercel.com/changelog",
      "statusCode": 200,
      "renderedWith": "lightpanda",
      "elapsedMs": 2340
    }
  }
}
```

`renderedWith` is now `"lightpanda"` (or `"chrome"` depending on your server config) — the browser rendered the page and the markdown has real content.

### Choosing a `waitFor` value

| Page type | Recommended `waitFor` |
|---|---|
| Light hydration (mostly SSR) | `500`–`1000` ms |
| Typical React/Next.js SPA | `2000` ms |
| Heavy client-side data fetch | `3000`–`5000` ms |

Start low and increase only when content is still missing. Long waits increase latency and cost; they do not fix bot-wall blocks.

---

## Step 3 — Pin a renderer

Once you know the site works with a specific renderer, hard-pin it to skip the auto-detect chain. Valid values for `renderer`:

| Value | Renderer used |
|---|---|
| `"auto"` (or omit) | Auto-detect chain — lightpanda first, falls back to chrome |
| `"lightpanda"` | LightPanda — fastest, best for SPAs without anti-bot |
| `"chrome"` | Full headless Chrome — heavier, handles more complex JS |
| `"chrome_proxy"` | Headless Chrome via residential proxy — for geo-blocked or heavily protected sites |
| `"playwright"` | Playwright driver — use when CDP direct is unavailable |

Pinning a non-`auto` renderer implies `renderJs: true`. If you also set `renderJs: false` explicitly, the pin is ignored and HTTP-only is used.

:::tabs
::tab{title="cURL — pin lightpanda"}
```bash
curl -s -X POST https://api.fastcrw.com/v1/scrape \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://vercel.com/changelog",
    "formats": ["markdown"],
    "renderer": "lightpanda",
    "waitFor": 2000
  }'
```
::tab{title="cURL — pin chrome"}
```bash
curl -s -X POST https://api.fastcrw.com/v1/scrape \
  -H "Authorization: Bearer YOUR_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://vercel.com/changelog",
    "formats": ["markdown"],
    "renderer": "chrome",
    "waitFor": 2000
  }'
```
::tab{title="Python (crw SDK)"}
```python
from crw import CrwClient

client = CrwClient()

# Pin to lightpanda (fast, no fallback)
result = client.scrape(
    "https://vercel.com/changelog",
    formats=["markdown"],
    renderer="lightpanda",
    wait_for=2000,
)

rendered_with = result["metadata"]["renderedWith"]
print(f"renderedWith: {rendered_with}")   # "lightpanda"
print(result["markdown"][:400])
```
:::

**Expected response when pinning `"chrome"`:**

```json
{
  "success": true,
  "data": {
    "markdown": "# Changelog\n\n## June 2026\n\n### Build output compression...",
    "metadata": {
      "title": "Changelog – Vercel",
      "sourceURL": "https://vercel.com/changelog",
      "statusCode": 200,
      "renderedWith": "chrome",
      "elapsedMs": 3120
    }
  }
}
```

If the pinned renderer is not configured on your server, the API returns HTTP 400:

```json
{
  "success": false,
  "error": "renderer 'chrome' not available; configured renderers: [lightpanda]. Update server config or omit the 'renderer' field.",
  "error_code": "invalid_request"
}
```

---

## Full diagnostic script (Python)

Paste and run this to walk through all three steps automatically:

```python
import sys
from crw import CrwClient

TARGET = "https://vercel.com/changelog"
client = CrwClient()  # reads CRW_API_KEY from env


def check(label: str, result: dict) -> None:
    md = result.get("markdown", "")
    rendered_with = result["metadata"].get("renderedWith", "?")
    elapsed = result["metadata"].get("elapsedMs", 0)
    print(f"\n{'='*60}")
    print(f"[{label}]  renderedWith={rendered_with}  elapsed={elapsed}ms")
    print(f"  markdown length: {len(md)} chars")
    if md:
        print(f"  preview: {md[:120].strip()!r}")
    else:
        print("  (empty — page needs JS rendering)")


# Step 1: plain HTTP
r1 = client.scrape(TARGET, formats=["markdown"])
check("Step 1 — plain HTTP", r1)

# Step 2: renderJs + waitFor
r2 = client.scrape(TARGET, formats=["markdown"], render_js=True, wait_for=2000)
check("Step 2 — renderJs=True, waitFor=2000", r2)

# Step 3: pin renderer explicitly
r3 = client.scrape(TARGET, formats=["markdown"], renderer="lightpanda", wait_for=2000)
check("Step 3 — renderer=lightpanda (pinned)", r3)

if len(r3.get("markdown", "")) > 200:
    print("\nDone — content looks good with pinned renderer.")
else:
    print("\nStill thin. Try renderer='chrome' or increase waitFor.", file=sys.stderr)
```

---

## What each field controls

| Field | Wire name | Type | Effect |
|---|---|---|---|
| `render_js` | `renderJs` | `bool \| null` | `null` = auto-detect, `true` = force browser, `false` = HTTP only |
| `renderer` | `renderer` | `string` | Pin to `"auto"`, `"lightpanda"`, `"chrome"`, `"chrome_proxy"`, or `"playwright"` |
| `wait_for` | `waitFor` | `integer` (ms) | Wait after load before extracting HTML |
| `metadata.renderedWith` | — | response field | Which renderer actually ran (`"http"`, `"lightpanda"`, `"chrome"`, `"chrome_proxy"`) |

---

## Troubleshooting

**`renderedWith` is `"http"` even with `renderJs: true`**
Check `warning`. If it starts with `js_escalation_failed:`, a renderer was
configured and tried but every tier failed (usually the deadline was too short
for a browser — raise `timeout`); the HTTP body is returned rather than an error
so you still get content. A PDF also legitimately reports `"http"`, because
there is no DOM to render. If `renderedWith` is `"http_only_fallback"` instead,
no JS renderer is configured at all: check your deployment or use the cloud
endpoint (`https://api.fastcrw.com`).

**Markdown is still empty after rendering**
The page may be behind an anti-bot wall. Look for a `warnings` array in the response — it will name the vendor (Cloudflare, Akamai, etc.). Switch to `renderer: "chrome_proxy"` with a `country` code to route through a residential IP.

**`error_code: "invalid_request"` when pinning**
The pinned renderer is not running on your server. The error message lists what is available. Either configure the renderer in `config.toml` or switch to `"auto"`.

**Content is a loading spinner, not real data**
Increase `waitFor`. Start at `2000`, try `3000`, then `5000`. Beyond 5 seconds the page is likely blocked or requires authentication, not just slow.

**`elapsedMs` is very high**
Only pin a browser renderer when HTTP-only clearly fails. Plain HTTP fetches take ~50–300 ms; a browser fetch with `waitFor: 2000` takes 2–5 s minimum.
