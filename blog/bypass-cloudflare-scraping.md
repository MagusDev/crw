# How to Scrape Cloudflare-Protected Sites with CRW's Stealth Mode

> CRW v0.0.11 adds automatic stealth JavaScript injection and Cloudflare challenge retry. Here's how it works under the hood, and how to configure it for maximum success rate.

**Published:** 2026-04-17  
**Updated:** 2026-05-23  
**Canonical:** https://fastcrw.com/blog/bypass-cloudflare-scraping

---

## The Cloudflare Problem

Cloudflare protects over 20% of all websites on the internet. If you're building a web scraper — whether for RAG pipelines, AI agents, price monitoring, or content aggregation — you will hit Cloudflare-protected sites. It's unavoidable.

Cloudflare's bot detection works in layers, each progressively harder to bypass:

1. **IP reputation** — known datacenter IPs, VPN ranges, and previously flagged IPs get challenged immediately
2. **TLS fingerprinting** — Cloudflare analyzes the TLS ClientHello message to identify automated HTTP clients (like curl or Python's requests library) that don't match a real browser's TLS profile
3. **JavaScript challenge** — a page that requires executing JavaScript to generate a challenge token. Bots without a JS engine fail here
4. **Browser fingerprinting** — JavaScript that checks `navigator.webdriver`, Chrome runtime objects, plugin arrays, and other browser properties that headless browsers typically get wrong
5. **Turnstile (CAPTCHA)** — interactive or non-interactive challenge that requires human-like interaction. This is the hardest layer and cannot be solved programmatically

Most scrapers fail at layer 3 or 4. They either don't have a JavaScript engine at all (HTTP-only scrapers), or they run a headless browser that leaks automation signals (Playwright, Puppeteer, Selenium with default settings).

CRW v0.0.11 tackles layers 1–4 (all except Turnstile CAPTCHA) with a combination of stealth JavaScript injection, automatic HTTP-to-CDP escalation, and Cloudflare challenge retry logic.

## How CRW's Stealth Mode Works

CRW's anti-bot bypass is a multi-stage pipeline that runs automatically when you scrape a URL. You don't need to configure anything — stealth mode is enabled by default when JS rendering is active.

### Stage 1: HTTP Attempt

CRW first tries a plain HTTP request with browser-like headers. This works for about 60–70% of websites, including most Cloudflare-protected sites that only use basic IP reputation checks:

- Browser-like `User-Agent` string (rotated from a pool of real Chrome/Firefox UAs)
- Standard browser headers: `Accept`, `Accept-Language`, `Accept-Encoding`, `Sec-Fetch-*`
- Proper `Referer` and `Origin` headers when applicable

If the HTTP response looks normal (200 status, reasonable content length, no challenge markers), CRW extracts the content and returns it. Fast path — no browser needed.

### Stage 2: Challenge Detection

CRW analyzes the HTTP response for Cloudflare challenge signatures:

- HTTP 403 with Cloudflare challenge page body
- HTTP 503 with `cf-mitigated: challenge` header
- HTML body containing `cf-browser-verification`, `cf_chl_opt`, or `turnstile` markers
- Meta refresh redirects to `/cdn-cgi/challenge-platform/`
- Empty or near-empty body with Cloudflare script tags

If any of these patterns are detected, CRW automatically escalates to browser rendering.

### Stage 3: Stealth Browser Rendering

This is where the magic happens. Before navigating to the page, CRW injects stealth JavaScript via Chrome DevTools Protocol's `Page.addScriptToEvaluateOnNewDocument`. This runs before any page JavaScript executes, meaning Cloudflare's detection scripts see a "real" browser environment.

The stealth injection patches these detection vectors:

#### `navigator.webdriver`

The most common headless browser detection. In a real browser, `navigator.webdriver` is `undefined` or `false`. In Puppeteer/Playwright/CDP, it's `true`. CRW patches it:

```
Object.defineProperty(navigator, 'webdriver', {
  get: () => undefined,
  configurable: true,
});
```

#### Chrome Runtime Object

Real Chrome browsers have a `window.chrome` object with specific properties. Headless Chrome often has a missing or incomplete `chrome` object. CRW creates a convincing mock:

```
window.chrome = {
  runtime: {
    onMessage: { addListener: function() {} },
    sendMessage: function() {},
    connect: function() { return { onMessage: { addListener: function() {} } }; },
  },
  loadTimes: function() { return {}; },
  csi: function() { return {}; },
};
```

#### Plugin and MimeType Arrays

Real browsers report installed plugins (PDF viewer, Chrome PDF Viewer, etc.). Headless browsers report zero plugins. CRW injects realistic plugin data:

```
Object.defineProperty(navigator, 'plugins', {
  get: () => {
    const plugins = [
      { name: 'Chrome PDF Plugin', filename: 'internal-pdf-viewer', description: 'Portable Document Format' },
      { name: 'Chrome PDF Viewer', filename: 'mhjfbmdgcfjbbpaeojofohoefgiehjai', description: '' },
      { name: 'Native Client', filename: 'internal-nacl-plugin', description: '' },
    ];
    plugins.length = 3;
    return plugins;
  },
});
```

#### Languages

Headless browsers often have empty or inconsistent language settings. CRW ensures `navigator.languages` returns a realistic value:

```
Object.defineProperty(navigator, 'languages', {
  get: () => ['en-US', 'en'],
});
```

#### Permissions API

The Permissions API behaves differently in automated browsers. CRW patches the `query` method to return realistic permission states for common permissions like notifications:

```
const originalQuery = window.navigator.permissions.query;
window.navigator.permissions.query = (parameters) => {
  if (parameters.name === 'notifications') {
    return Promise.resolve({ state: Notification.permission });
  }
  return originalQuery(parameters);
};
```

### Stage 4: Cloudflare Challenge Retry

Even with stealth injection, Cloudflare's non-interactive JavaScript challenge takes a few seconds to solve. The challenge page runs computations in the browser, generates a token, and automatically redirects to the real page.

CRW handles this with a retry loop:

1. Navigate to the page with stealth injection active
2. Check if the page is a Cloudflare challenge
3. If yes, wait 3 seconds for the challenge to auto-resolve
4. Check again — repeat up to 3 times (total 9 seconds max)
5. If the challenge resolves, extract content from the final page
6. If it doesn't resolve after 3 attempts, return the best content available

This 3×3s retry pattern handles the vast majority of Cloudflare JavaScript challenges without user intervention.

### Stage 5: Chrome Failover

CRW's rendering pipeline has a full failover chain: **HTTP → LightPanda → Chrome**. If LightPanda can't render a page (some complex SPAs with heavy WebGL or WebAssembly), Chrome takes over automatically.

This is relevant for Cloudflare because some challenge implementations use advanced browser APIs that LightPanda doesn't support. Chrome, being a full browser engine, handles these cases.

## Setting Up Stealth Scraping

Stealth mode requires JS rendering. Here's the complete setup:

```
# Install CRW
cargo install crw-server

# Set up JS rendering (downloads LightPanda)
crw-server setup

# Start LightPanda in the background
lightpanda serve --host 127.0.0.1 --port 9222 --block-private-networks &

# Start CRW
crw-server
```

That's it. Stealth injection is enabled by default whenever CRW uses the browser renderer. No flags, no config options — it's always on.

### Adding Chrome as a Failover

For maximum success rate, add Chrome as a fallback renderer:

```
# Docker Compose with both LightPanda and Chrome
docker compose up
```

CRW's `docker-compose.yml` includes both LightPanda and Chrome (via chromedp/headless-shell) as sidecars. The failover chain runs automatically.

For manual setup without Docker:

```
# Install Chrome
apt install -y google-chrome-stable

# Run headless Chrome
google-chrome --headless --remote-debugging-port=9223 --no-sandbox &
# ⚠️ --no-sandbox disables Chrome's security sandbox.
# Only use in containers or isolated environments.
# On a host system, omit --no-sandbox and run as non-root.

# Configure CRW to use Chrome as failover
cat >> config.local.toml << 'EOF'
[renderer]
mode = "auto"
chrome_ws_url = "ws://127.0.0.1:9223"
EOF
```

## Adding Proxies for IP Reputation

Stealth mode handles browser fingerprinting (layers 3–4), but IP reputation (layer 1) is a separate challenge. If Cloudflare blocks your server's IP, no amount of stealth JavaScript will help.

CRW supports per-request proxy configuration:

```
# Global proxy in config
[proxy]
url = "http://user:pass@proxy.example.com:8080"

# Per-request proxy via API
curl -X POST http://localhost:3000/v1/scrape \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://cloudflare-protected-site.com",
    "proxy": "http://user:pass@residential-proxy.com:8080"
  }'
```

### Proxy Types and When to Use Each

| Proxy Type | Cost | CF Bypass Rate | Best For |
| --- | --- | --- | --- |
| Datacenter | $1–3/GB | Low (30–50%) | Non-CF sites, high volume |
| ISP/Static residential | $3–8/GB | Medium (60–80%) | Moderate CF protection |
| Rotating residential | $5–15/GB | High (85–95%) | Strong CF protection |
| Mobile | $15–30/GB | Very high (95%+) | Hardest CF sites |

For most Cloudflare-protected sites, rotating residential proxies combined with CRW's stealth mode give a 90%+ success rate. The managed [fastCRW cloud](https://fastcrw.com) includes a built-in residential proxy network, so you don't need to source and manage proxies yourself.

## What CRW Can and Can't Bypass

Let's be honest about the limitations:

### CRW Handles Well

- **Cloudflare JavaScript challenges (non-interactive)** — auto-solved via stealth + retry
- **Basic bot detection** — navigator.webdriver, plugin checks, language checks
- **UA/header fingerprinting** — browser-like header rotation
- **HTTP-to-JS escalation** — automatic switch from HTTP to browser when needed
- **Challenge pages that auto-resolve** — the 3×3s retry handles these reliably

### CRW Can't Bypass

- **Cloudflare Turnstile (interactive CAPTCHA)** — requires human interaction. No scraper can solve this programmatically without a CAPTCHA solving service.
- **Cloudflare Under Attack Mode** — sites in active DDoS mitigation have extremely aggressive checks that block most automated access
- **Canvas/WebGL fingerprinting** — some advanced bot detection analyzes GPU rendering output. LightPanda doesn't support this; Chrome handles it better.
- **Behavioral analysis** — Cloudflare analyzes mouse movements, scroll patterns, and timing. Automated scraping doesn't generate realistic behavioral signals.

The key insight: CRW maximizes your success rate on the sites that are technically bypassable, and fails fast on the ones that aren't. You don't waste time waiting for timeouts on impossible targets.

## The HTTP → CDP Auto-Escalation Pipeline

One of CRW's most useful features for Cloudflare is the automatic escalation from HTTP to CDP (Chrome DevTools Protocol) rendering. Here's how the decision tree works:

```
Request arrives
  │
  ├─ Try HTTP fetch with browser-like headers
  │    │
  │    ├─ 200 + content → extract markdown → return ✓
  │    │
  │    ├─ 403/503 + CF challenge detected
  │    │    │
  │    │    └─ Escalate to CDP rendering
  │    │         │
  │    │         ├─ Inject stealth JS
  │    │         ├─ Navigate to URL
  │    │         ├─ CF challenge detected?
  │    │         │    ├─ Wait 3s, retry (up to 3x)
  │    │         │    └─ Challenge resolved → extract → return ✓
  │    │         │
  │    │         ├─ LightPanda fails?
  │    │         │    └─ Failover to Chrome → retry
  │    │         │
  │    │         └─ Content loaded → extract → return ✓
  │    │
  │    └─ Other error → return error
  │
  └─ Done
```

This pipeline means you never need to decide whether a site needs JS rendering. CRW tries the fast path (HTTP) first and only escalates when necessary. For sites that don't use Cloudflare, you get the performance of a plain HTTP scraper. For sites that do, you get automatic stealth rendering with zero configuration.

## Using Stealth Scraping with AI Agents

When CRW is connected to Claude Code via MCP, the stealth pipeline runs automatically on every scrape request. The AI agent doesn't need to know about Cloudflare — it just asks to scrape a URL and gets clean content back.

```
# Connect CRW to Claude Code (with JS rendering server)
claude mcp add fastcrw -e CRW_API_URL=http://localhost:3000 -- npx -y crw-mcp
```

Now when you tell Claude Code to scrape a Cloudflare-protected site, the pipeline handles the challenge transparently:

```
You: "Scrape https://cloudflare-protected-docs.com/api/authentication
     and show me how their OAuth flow works."

Claude Code:
  → calls crw_scrape (via MCP)
  → CRW: HTTP → 403 CF challenge detected
  → CRW: escalate to LightPanda + stealth JS
  → CRW: challenge auto-resolved after 3 seconds
  → CRW: returns clean markdown
  → Claude Code reads the content and explains the OAuth flow
```

The entire anti-bot pipeline is invisible to both the user and the AI agent. That's the design goal: make Cloudflare a non-issue for legitimate scraping use cases.

## Benchmarks: Stealth Mode Success Rates

We tested CRW v0.0.11's stealth mode against 200 Cloudflare-protected sites across different protection levels:

| Protection Level | Sites Tested | CRW (HTTP only) | CRW (Stealth + LightPanda) | CRW (Stealth + Chrome) |
| --- | --- | --- | --- | --- |
| CF Free (basic) | 80 | 72% | 95% | 97% |
| CF Pro | 60 | 35% | 82% | 89% |
| CF Business | 40 | 15% | 65% | 78% |
| CF Enterprise | 20 | 5% | 40% | 55% |

Key takeaways:

- Stealth + Chrome more than doubles the success rate compared to HTTP-only scraping
- CF Free/Pro sites (the vast majority) are reliably scraped at 82–97%
- CF Enterprise sites often require residential proxies for acceptable success rates
- Adding residential proxies to Stealth + Chrome pushes CF Business/Enterprise to 85–95%

## Ethical Considerations

CRW's stealth mode is designed for legitimate scraping use cases: reading documentation, monitoring public pricing, aggregating public content, and powering AI agents that need web access. It is not designed for:

- Scraping personal data without consent
- Circumventing paywalls or access controls
- DDoS or high-volume attacks against protected sites
- Scraping sites that explicitly prohibit it in their ToS (check robots.txt)

CRW respects robots.txt by default. If a site's robots.txt disallows scraping, CRW will refuse to scrape it unless you explicitly override this behavior. We believe scraping should be a tool for legitimate access to public information, not a weapon for abuse.

## Related Guides

- [Add Web Scraping to Claude Code in 30 Seconds](/blog/claude-code-web-scraping) — use stealth scraping from your AI agent
- [$5 VPS Web Scraping](/blog/crw-on-5-dollar-vps) — deploy CRW with stealth mode on a budget server
- [Full MCP Setup Guide](/blog/mcp-web-scraping) — connect CRW to any MCP client

## FAQ

### Can CRW bypass Cloudflare?

CRW can bypass Cloudflare's non-interactive JavaScript challenges and basic bot detection through automatic stealth JavaScript injection. In our 200-site test it handled Cloudflare Free and Pro protection at 82–97% success with stealth plus Chrome. For Enterprise-level protection, residential proxies significantly improve results. CRW cannot bypass Cloudflare Turnstile (interactive CAPTCHA) or Under Attack Mode.

### How does CRW's stealth mode work?

CRW injects JavaScript before any page scripts run via CDP's Page.addScriptToEvaluateOnNewDocument. This patches the browser properties bot detection checks: navigator.webdriver, the Chrome runtime object, plugin and mimeType arrays, language settings, and the Permissions API. The patching makes a headless browser look like a real user's browser to Cloudflare's detection scripts.

### Do I need to configure stealth mode?

No. Stealth mode is enabled by default whenever CRW uses browser rendering with LightPanda or Chrome — there are no flags or config options for it. The only setup required is enabling JS rendering with crw-server setup. The stealth injection, Cloudflare challenge detection, and the 3×3s retry loop all run automatically.

### What's the difference between LightPanda and Chrome for Cloudflare scraping?

LightPanda is lighter and faster to start but implements only a subset of browser APIs. Chrome is heavier but has full browser compatibility, so it passes canvas and WebGL fingerprinting that LightPanda cannot. For Cloudflare specifically, Chrome has roughly a 10% higher success rate. CRW tries LightPanda first and fails over to Chrome automatically.

### Should I use proxies with CRW's stealth mode?

It depends on the site. For Cloudflare Free and Pro, stealth mode alone is often enough. For Cloudflare Business and Enterprise, residential proxies meaningfully raise success rates — adding them to stealth plus Chrome pushes Business/Enterprise sites to 85–95%. CRW supports per-request proxy configuration, so you can reserve proxies for the hardest targets and save bandwidth on easier ones.

### Is it legal to bypass Cloudflare?

This is a legal gray area that varies by jurisdiction. Accessing publicly available information is generally legal in most jurisdictions, but circumventing access controls on non-public content may violate the CFAA in the US or similar laws elsewhere. CRW respects robots.txt by default and will refuse a disallowed scrape unless you explicitly override it. Always check a site's Terms of Service, and when in doubt consult a lawyer familiar with your jurisdiction's computer access laws.
