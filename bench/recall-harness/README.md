# recall@budget harness

Offline measurement of where `/v1/search` answer quality is lost: **retrieval**
(is the answer in the scraped markdown at all), **selection** (does it survive
into the per-source window), or downstream. Removes prod latency variance, which
has repeatedly swamped sampled A/B runs — the same code measured 80% and 52% on
different runs. Runs in under a second, fully reproducible.

## 1. Freeze a slice (one slow step, hits prod once)

```bash
CRW_KEY=<key> CRW_BASE=https://fastcrw.com/api \
  bun run capture.ts 50 5 cap50.jsonl
```

Captures N SimpleQA questions: for each, the search result set with full scraped
markdown per source (`answer:false`, so it is pure retrieval). One line per
question: `{i, q, gold, sources:[{url,title,description,markdown}], ms}`.

## 2. Replay through the real selection code (fast, offline, repeatable)

The harness is an `#[ignore]`d test in `crates/crw-extract/src/answer.rs`
(`recall_at_budget`) so it can reach the private `select_relevant_passages`
without widening the API:

```bash
RB_CAP=/abs/path/cap50.jsonl RB_BUDGET=8192 \
  cargo test -p crw-extract --release recall_at_budget -- --nocapture --ignored
```

Reports, for the frozen set:

- **retrieval ceiling** — gold present anywhere in any source's markdown
  (all-tokens match; strict-phrase also printed)
- **survives head-truncation** vs **survives BM25 selection** — the two window
  strategies, so selection loss is isolated from retrieval loss
- **dropped by SELECTION** — retrieval had it, the window strategy lost it
  (fixable in-window: scoring, budget, chunking)
- **never RETRIEVED** — search/scrape never produced it (fixable upstream:
  see the domain empty-markdown breakdown; this is where anti-bot blocks show up)

`RB_BUDGET` sweeps the per-source cap (`DEFAULT_MAX_CHARS_PER_SOURCE` is 8192)
without touching prod.
