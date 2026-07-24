// Capture: freeze prod search+scrape output for a SimpleQA slice so the SELECTION layer
// can be replayed offline, deterministically, without prod latency variance.
// Usage: CRW_KEY=.. bun run capture.ts <N> <conc> <out.jsonl>
const env = (k: string) => { const v = process.env[k]; if (!v) throw new Error(`missing ${k}`); return v; };
const CRW_KEY = env("CRW_KEY"), CRW_BASE = process.env.CRW_BASE ?? "https://fastcrw.com/api";
const N = Number(process.argv[2] ?? 50);
const CONC = Number(process.argv[3] ?? 4);
const OUT = process.argv[4] ?? "cap.jsonl";

async function search(q: string): Promise<any> {
  for (let a = 0; a < 3; a++) {
    try {
      const r = await fetch(`${CRW_BASE}/v1/search`, {
        method: "POST",
        headers: { "content-type": "application/json", authorization: `Bearer ${CRW_KEY}` },
        // answer:false -> pure retrieval. maxCharsPerSource high so we capture the FULL scraped
        // markdown and can apply any budget offline.
        body: JSON.stringify({
          query: q, limit: 5,
          scrapeOptions: { formats: ["markdown"], onlyMainContent: true },
        }),
        signal: AbortSignal.timeout(180_000),
      });
      if (r.status >= 500 || r.status === 429) { await Bun.sleep(2000 * (a + 1)); continue; }
      if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
      return r.json();
    } catch (e) { if (a === 2) throw e; await Bun.sleep(2000 * (a + 1)); }
  }
}

async function fetchRows(n: number) {
  const r = await fetch(
    `https://datasets-server.huggingface.co/rows?dataset=basicv8vc/SimpleQA&config=default&split=test&offset=0&length=${n}`,
    { signal: AbortSignal.timeout(30_000) });
  const j = await r.json();
  return (j.rows as any[]).map((x) => ({ q: x.row.problem as string, gold: x.row.answer as string }));
}

const rows = await fetchRows(N);
const out: string[] = [];
let idx = 0, done = 0;
async function worker() {
  while (idx < rows.length) {
    const i = idx++; const row = rows[i]; const t = Date.now();
    let sources: any[] = [], err = "";
    try {
      const r = await search(row.q);
      const arr = r?.data?.web ?? r?.web ?? r?.data ?? [];
      sources = (Array.isArray(arr) ? arr : []).map((s: any) => ({
        url: s.url ?? "", title: s.title ?? "",
        description: s.description ?? "",
        markdown: s.markdown ?? "",
      }));
    } catch (e) { err = String(e); }
    out.push(JSON.stringify({ i, q: row.q, gold: row.gold, sources, err, ms: Date.now() - t }));
    done++;
    console.log(`[${done}/${rows.length}] ${((Date.now() - t) / 1000).toFixed(1)}s  src=${sources.length}  md=${sources.reduce((a, s) => a + s.markdown.length, 0)}  ${err ? "ERR " + err.slice(0, 60) : ""}  ${row.q.slice(0, 50)}`);
  }
}
await Promise.all(Array.from({ length: CONC }, () => worker()));
await Bun.write(OUT, out.sort((a, b) => JSON.parse(a).i - JSON.parse(b).i).join("\n") + "\n");
console.log(`\nwrote ${out.length} rows -> ${OUT}`);
