//! HTML table normalization: `rowspan`/`colspan` expansion into a flat grid,
//! applied BEFORE the HTML reaches `htmd`.
//!
//! GFM pipe tables have no concept of a merged cell. htmd emits one pipe cell
//! per `<td>`, so every row below a `rowspan` is short by one column and the
//! values slide left under the wrong headers. Expanding the spans first is what
//! makes the emitted table row-addressable.
//!
//! Deliberately narrow: only tables that already carry a real header (`<thead>`
//! or a leading run of all-`<th>` rows) are rewritten. Synthesizing a header for
//! a headerless table was measured on a 15-page corpus (46 data tables) and
//! fired exactly once, on Hacker News' layout grid, so it is not attempted.
//!
//! Two passes, mirroring the `scraper` (read) + `lol_html` (rewrite) split
//! already used in `clean.rs`:
//!
//! - Pass A (`scraper`, read-only): parse the document, expand `rowspan`/
//!   `colspan` into a full grid for every top-level data `<table>`, and
//!   serialize a fresh, span-free `<table>` string.
//! - Pass B (`lol_html`, streaming rewrite): splice each top-level table's
//!   fresh HTML back into the document in place, correlating by document-order
//!   index rather than string matching (byte-identical duplicate tables would
//!   collide on a string search). A depth counter, required because
//!   `lol_html` still dispatches the `table` element handler for elements
//!   inside a subtree already marked for replacement (verified by
//!   `nested_table_lol_html_handler_fires_but_is_a_noop` below), makes sure
//!   only depth-0 tables are ever replaced; nested tables are always no-ops
//!   and stay opaque HTML inside their parent cell.
//!
//! The index correlation only holds while both parsers agree on how many
//! top-level tables the document has, which tag soup can break: html5ever
//! auto-closes an unclosed `<table>` and reports the next one as a sibling,
//! while `lol_html`'s raw token stream sees it as nested. Pass B therefore
//! counts the depth-0 tables it visited and discards the whole splice on any
//! disagreement, returning the original bytes rather than silently swallowing
//! a table.
//!
//! Gated entirely behind `ExtractionConfig::normalize_tables`; see
//! `markdown::html_to_markdown_with`.

use crate::tables;
use lol_html::html_content::{ContentType, EndTag};
use lol_html::{HandlerResult, RewriteStrSettings, doc_comments, element, rewrite_str};
use scraper::{ElementRef, Html, Selector};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;

/// Upper bound on a single `rowspan`/`colspan` value, so one absurd attribute
/// (`rowspan="999999999"`) can't allocate a huge row on its own. This does NOT
/// bound the grid: N rows each carrying a colliding `rowspan` grow the column
/// count linearly with N, so total cells grow quadratically even at the clamp
/// value. `MAX_GRID_CELLS` is what actually bounds the grid.
const MAX_SPAN: usize = 1000;

/// Hard ceiling on total expanded grid cells (rows x columns) for one table.
/// Above it the table is left untouched rather than expanded, 50k cells is
/// already far beyond any real document table (1000 rows x 50 columns), while
/// a quadratic blow-up crosses it almost immediately.
const MAX_GRID_CELLS: usize = 50_000;

/// Elements that carry no tabular content but do get serialized by
/// `inner_html()`. Wikipedia inlines `<style>` TemplateStyles blocks *inside*
/// table cells; captured as a cell value, one such block became a 64 KB
/// "datum", and htmd pads every cell in a column out to that column's widest
/// cell, so 151 rows each grew a ~64 KB run of spaces (609 KB -> 7.85 MB,
/// 97% of it whitespace).
const CELL_NON_CONTENT_TAGS: [&str; 4] = ["style", "script", "noscript", "template"];

/// Cheap trip-wire: only cells whose serialized HTML contains one of these
/// substrings pay for the rewrite pass above. Text nodes are escaped on
/// serialization, so a literal `<style` can only be a real tag.
const CELL_NON_CONTENT_MARKERS: [&str; 5] = ["<style", "<script", "<noscript", "<template", "<!--"];

/// Upper bound on ONE cell's extracted HTML (after non-content stripping). A
/// real table datum is a number, a name, a short sentence; kilobytes of markup
/// in a single cell means the cell captured something that is not data, and
/// because htmd pads a column to its widest cell the cost is paid by every row.
/// Over the ceiling the whole table is left untouched rather than truncated
/// mid-markup, truncation would emit broken HTML. 4 KB is far above any real
/// datum yet well below the runaway sizes observed on real pages.
const MAX_CELL_HTML: usize = 4096;

/// Ceiling on the TOTAL expanded cell HTML for one table. `MAX_GRID_CELLS` and
/// `MAX_CELL_HTML` bound count and per-cell size separately, and their product
/// still allows roughly 200 MB. 8 MB is far past any real document table.
const MAX_GRID_BYTES: usize = 8 * 1024 * 1024;

/// A cell's inner HTML with non-content elements and comments removed.
fn cell_inner_html(cell: ElementRef<'_>) -> String {
    let raw = cell.inner_html();
    if !CELL_NON_CONTENT_MARKERS.iter().any(|m| raw.contains(m)) {
        return raw;
    }
    let settings = CELL_NON_CONTENT_TAGS
        .iter()
        .fold(RewriteStrSettings::new(), |s, tag| {
            s.append_element_content_handler(element!(*tag, |el| {
                el.remove();
                Ok(())
            }))
        })
        .append_document_content_handler(doc_comments!(|c| {
            c.remove();
            Ok(())
        }));
    match rewrite_str(&raw, settings) {
        Ok(stripped) => stripped,
        Err(_) => raw,
    }
}

/// Normalize every top-level `<table>` in `html`, leaving everything else ,
/// including nested tables, byte-for-byte untouched.
pub fn normalize_tables(html: &str) -> String {
    let replacements = build_replacements(html);
    if replacements.iter().all(Option::is_none) {
        return html.to_string();
    }
    splice_tables(html, replacements)
}

// ─── Pass A: read + grid-expand (scraper) ──────────────────────────────────

fn build_replacements(html: &str) -> Vec<Option<String>> {
    let doc = Html::parse_document(html);
    let table_sel = Selector::parse("table").unwrap();

    doc.select(&table_sel)
        .filter(|table| !is_nested(*table))
        .map(|table| {
            // Layout tables (nav shells, newsletter grids) are left alone: they
            // carry no tabular meaning and rewriting them only churns bytes.
            tables::is_likely_data_table(table)
                .then(|| build_replacement(table))
                .flatten()
        })
        .collect()
}

fn is_nested(table: ElementRef<'_>) -> bool {
    table.ancestors().any(|node| {
        node.value()
            .as_element()
            .is_some_and(|el| el.name() == "table")
    })
}

#[derive(Debug, Clone)]
struct RawCell {
    html: String,
    is_th: bool,
    rowspan: usize,
    colspan: usize,
}

#[derive(Debug, Clone)]
struct GridCell {
    html: String,
    is_th: bool,
}

fn build_replacement(table: ElementRef<'_>) -> Option<String> {
    let caption = table
        .child_elements()
        .find(|c| c.value().name() == "caption")
        .map(|c| c.html());

    let mut thead_raw_rows: Vec<Vec<RawCell>> = Vec::new();
    let mut body_raw_rows: Vec<Vec<RawCell>> = Vec::new();

    for child in table.child_elements() {
        match child.value().name() {
            "thead" => {
                for tr in child.child_elements().filter(|c| c.value().name() == "tr") {
                    thead_raw_rows.push(extract_cells(tr));
                }
            }
            "tbody" | "tfoot" => {
                for tr in child.child_elements().filter(|c| c.value().name() == "tr") {
                    body_raw_rows.push(extract_cells(tr));
                }
            }
            "tr" => body_raw_rows.push(extract_cells(child)),
            _ => {}
        }
    }

    if thead_raw_rows.is_empty() && body_raw_rows.is_empty() {
        return None;
    }

    // One oversized cell poisons the whole column (see `MAX_CELL_HTML`), so
    // bail out on the table as a whole instead of shipping a padded monster.
    if thead_raw_rows
        .iter()
        .chain(body_raw_rows.iter())
        .flatten()
        .any(|c| c.html.len() > MAX_CELL_HTML)
    {
        return None;
    }

    // The banner rule needs the table's true width, and a row's own colspans do
    // not give it: a `rowspan` carried down from an earlier row occupies a
    // column this row never mentions. Probing with width 0 disables the rule
    // (it requires `full_width > 1`) so this pass reuses the real column
    // accounting instead of a second, drift-prone copy of it.
    let probe_width = expand_grid(&thead_raw_rows, 0)?
        .iter()
        .chain(expand_grid(&body_raw_rows, 0)?.iter())
        .map(Vec::len)
        .max()
        .unwrap_or(0);

    let mut thead_grid = expand_grid(&thead_raw_rows, probe_width)?;
    let mut body_grid = expand_grid(&body_raw_rows, probe_width)?;

    // Reconcile the two sections: each grid was only rectangular within itself,
    // so a body row wider than every thead row (or vice versa) would otherwise
    // serialize with cells that have no column in the other section, htmd then
    // drops the overflow entirely.
    let width = thead_grid
        .iter()
        .chain(body_grid.iter())
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    if width.saturating_mul(thead_grid.len() + body_grid.len()) > MAX_GRID_CELLS {
        return None;
    }
    pad_rows_to(&mut thead_grid, width);
    pad_rows_to(&mut body_grid, width);

    // Only tables that already declare a header are rewritten. Without one
    // there is nothing to align the expanded columns against, and inventing a
    // header row is worse than leaving the table as htmd found it.
    let (header, body) = if !thead_grid.is_empty() {
        (flatten_header_rows(&thead_grid), body_grid)
    } else {
        leading_all_th_rows(&body_grid)?
    };

    Some(serialize_table(caption.as_deref(), header, body))
}

fn extract_cells(tr: ElementRef<'_>) -> Vec<RawCell> {
    tr.child_elements()
        .filter(|c| matches!(c.value().name(), "td" | "th"))
        .map(|c| RawCell {
            html: cell_inner_html(c),
            is_th: c.value().name() == "th",
            rowspan: parse_span(c.value().attr("rowspan")),
            colspan: parse_span(c.value().attr("colspan")),
        })
        .collect()
}

/// Drain contiguous pending rowspan cells starting at `col`, placing each into
/// `row_out` and advancing `col` past it. Stops at the first column with no
/// pending cell.
fn drain_pending_at(
    pending: &mut BTreeMap<usize, (String, bool, usize)>,
    col: &mut usize,
    row_out: &mut Vec<GridCell>,
) {
    while let Some((html, is_th, rows_left)) = pending.get(col).cloned() {
        row_out.push(GridCell {
            html: html.clone(),
            is_th,
        });
        if rows_left <= 1 {
            pending.remove(col);
        } else {
            pending.insert(*col, (html, is_th, rows_left - 1));
        }
        *col += 1;
    }
}

fn parse_span(v: Option<&str>) -> usize {
    v.and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|&n| n > 0) // colspan="0" / negative / unparseable → treat as 1
        .unwrap_or(1)
        .min(MAX_SPAN)
}

/// Grid expansion (pandas `_expand_colspan_rowspan` shape, hardened): a
/// pending-cell map keyed by column, drained before EVERY column is consumed
/// so a rowspan already occupying a column always wins over a new real cell
/// landing there (pandas #58461 / #59721, a colliding real cell is pushed to
/// the next free column instead of overwriting the pending span). The drain
/// has to run per column, not per cell: a `colspan` walking across a pending
/// rowspan's column would otherwise consume the slot the span owns, dropping
/// the spanned value from this row and re-emitting it a row later.
///
/// Returns `None` when the expanded grid would exceed `MAX_GRID_CELLS`, the
/// caller then leaves the table untouched instead of materializing it.
/// `full_width` is the table's true column count, used only to tell a
/// full-width banner cell from a narrower header-group cell. Pass 0 to disable
/// that distinction (used by the width probe in `build_replacement`).
fn expand_grid(rows: &[Vec<RawCell>], full_width: usize) -> Option<Vec<Vec<GridCell>>> {
    let mut pending: BTreeMap<usize, (String, bool, usize)> = BTreeMap::new();
    let mut grid = Vec::with_capacity(rows.len());
    let mut total_cells = 0usize;
    let mut total_bytes = 0usize;

    for raw_row in rows {
        let mut row_out = Vec::new();
        let mut col = 0usize;

        for cell in raw_row {
            // Only a `<th>` spanning a SUBSET of the columns is a group label,
            // which genuinely belongs to each sub-column ("Group / Sub").
            // A full-width `<th>` is a banner (repeating one turned an
            // 850-byte election banner into a 7,651-byte line), and a spanning
            // `<td>` is ONE merged datum: copying "240" across Q1 and Q2 states
            // that each quarter was 240, which the page never said.
            let is_banner = full_width > 1 && cell.colspan >= full_width;
            let repeat_across = cell.is_th && !is_banner;
            for span_idx in 0..cell.colspan {
                // A pending span always wins the slot it reserved, so drain
                // before consuming each individual column.
                drain_pending_at(&mut pending, &mut col, &mut row_out);

                let html = if span_idx > 0 && !repeat_across {
                    String::new()
                } else {
                    cell.html.clone()
                };
                row_out.push(GridCell {
                    html: html.clone(),
                    is_th: cell.is_th,
                });
                if cell.rowspan > 1 {
                    pending.insert(col, (html, cell.is_th, cell.rowspan - 1));
                }
                col += 1;
                // Bound the row as it grows, not just once it is finished: a
                // single `<tr>` of 100 `<td colspan="1000">` is 2 KB of markup
                // that would otherwise build 100k cells before any check ran.
                if total_cells + row_out.len() > MAX_GRID_CELLS {
                    return None;
                }
            }
        }

        // Drain the tail: trailing rowspans continuing past the last real
        // cell in this row (contiguous from where the real cells left off).
        drain_pending_at(&mut pending, &mut col, &mut row_out);

        total_cells += row_out.len();
        total_bytes += row_out.iter().map(|c| c.html.len()).sum::<usize>();
        // `MAX_GRID_CELLS` and `MAX_CELL_HTML` bound cells and per-cell size
        // independently, so their product (50k x 4 KB) still permits a ~200 MB
        // table. Expansion duplicates cell HTML, so the total is what matters.
        if total_cells > MAX_GRID_CELLS || total_bytes > MAX_GRID_BYTES {
            return None;
        }
        grid.push(row_out);
    }

    Some(grid)
}

/// Rectangularize: pad every row out to `width` with empty cells.
fn pad_rows_to(grid: &mut [Vec<GridCell>], width: usize) {
    for row in grid.iter_mut() {
        while row.len() < width {
            row.push(GridCell {
                html: String::new(),
                is_th: false,
            });
        }
    }
}

/// GFM has no multi-row header: flatten N header rows into one, joining each
/// column's distinct non-empty values (e.g. "Group / Sub").
fn flatten_header_rows(rows: &[Vec<GridCell>]) -> Vec<GridCell> {
    if rows.len() <= 1 {
        return rows.first().cloned().unwrap_or_default();
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    (0..width)
        .map(|col| {
            let mut parts: Vec<String> = Vec::new();
            for row in rows {
                if let Some(cell) = row.get(col) {
                    let text = cell.html.trim();
                    if !text.is_empty() && parts.last().map(|p| p != text).unwrap_or(true) {
                        parts.push(text.to_string());
                    }
                }
            }
            GridCell {
                html: parts.join(" / "),
                is_th: true,
            }
        })
        .collect()
}

/// If the body grid opens with a contiguous run of all-`<th>` rows (no real
/// `<thead>` wrapper, but explicit header markup all the same), promote them.
fn leading_all_th_rows(body_grid: &[Vec<GridCell>]) -> Option<(Vec<GridCell>, Vec<Vec<GridCell>>)> {
    let split = body_grid
        .iter()
        .position(|row| row.is_empty() || !row.iter().all(|c| c.is_th))?;
    if split == 0 {
        return None;
    }
    let header = flatten_header_rows(&body_grid[..split]);
    let body = body_grid[split..].to_vec();
    Some((header, body))
}

fn serialize_table(
    caption: Option<&str>,
    header: Vec<GridCell>,
    body: Vec<Vec<GridCell>>,
) -> String {
    let mut out = String::from("<table>");
    if let Some(cap) = caption {
        out.push_str(cap);
    }
    out.push_str("<thead><tr>");
    for cell in header {
        out.push_str("<th>");
        out.push_str(&cell.html);
        out.push_str("</th>");
    }
    out.push_str("</tr></thead>");
    out.push_str("<tbody>");
    for row in body {
        out.push_str("<tr>");
        for cell in row {
            out.push_str("<td>");
            out.push_str(&cell.html);
            out.push_str("</td>");
        }
        out.push_str("</tr>");
    }
    out.push_str("</tbody></table>");
    out
}

// ─── Pass B: rewrite (lol_html) ─────────────────────────────────────────────

fn splice_tables(html: &str, replacements: Vec<Option<String>>) -> String {
    let depth = Rc::new(Cell::new(0u32));
    let index = Rc::new(Cell::new(0usize));
    let seen = Rc::clone(&index);
    let expected = replacements.len();

    let handler = element!("table", move |el| {
        let d = depth.get();
        if d == 0 {
            let i = index.get();
            if let Some(Some(replacement)) = replacements.get(i) {
                el.replace(replacement, ContentType::Html);
            }
            index.set(i + 1);
        }
        depth.set(d + 1);

        let depth_end = Rc::clone(&depth);
        let end_handler: Box<dyn FnOnce(&mut EndTag<'_>) -> HandlerResult + 'static> =
            Box::new(move |_end| {
                depth_end.set(depth_end.get().saturating_sub(1));
                Ok(())
            });
        el.on_end_tag(end_handler)?;

        Ok(())
    });

    let settings = RewriteStrSettings::new().append_element_content_handler(handler);
    let Ok(out) = rewrite_str(html, settings) else {
        return html.to_string();
    };

    // Fail-safe: the two parsers must agree on how many top-level tables the
    // document has. On tag soup (an unclosed `<table>` followed by another)
    // html5ever auto-closes and reports two siblings while `lol_html`'s raw
    // token stream sees the second nested inside the first, replacing the
    // first would then swallow the second table's content outright. Any
    // disagreement means the index correlation is unsound, so drop the whole
    // splice and hand back the original bytes.
    if seen.get() != expected {
        return html.to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalized_first_table_html(html: &str) -> String {
        let out = normalize_tables(html);
        let doc = Html::parse_document(&out);
        let sel = Selector::parse("table").unwrap();
        doc.select(&sel).next().expect("no table in output").html()
    }

    /// A table with no header markup is left byte-for-byte alone. Synthesizing
    /// a header for one was measured across 46 real data tables and fired once,
    /// on a site's layout grid, so row 0 is never promoted to column names.
    #[test]
    fn headerless_table_is_left_untouched() {
        let html = r#"<table>
            <tr><td scope="row">Revenue</td><td>100</td><td>200</td></tr>
            <tr><td scope="row">Costs</td><td>50</td><td>60</td></tr>
            <tr><td scope="row">Profit</td><td>50</td><td>140</td></tr>
        </table>"#;
        let out = normalize_tables(html);
        assert_eq!(out, html, "headerless table must not be rewritten");
    }

    /// C1: thead and body grids were rectangularized independently, so a body
    /// row wider than the header lost its overflow cells.
    #[test]
    fn header_and_body_widths_are_reconciled() {
        let html = r#"<table>
            <thead><tr><th>Name</th><th>Score</th></tr></thead>
            <tbody><tr><td>Alice</td><td>90</td><td>Bonus note</td></tr></tbody>
        </table>"#;
        let out = normalized_first_table_html(html);
        assert!(out.contains("Bonus note"), "wide body cell dropped: {out}");
        let doc = Html::parse_document(&out);
        let th = doc.select(&Selector::parse("thead th").unwrap()).count();
        let td = doc.select(&Selector::parse("tbody td").unwrap()).count();
        assert_eq!(
            (th, td),
            (3, 3),
            "header and body must share a width: {out}"
        );
    }

    /// C3: the pending-rowspan drain ran once per CELL, so a `colspan` walking
    /// across a pending span's column consumed that slot, the spanned value
    /// vanished from its row and reappeared a row later.
    #[test]
    fn colspan_crossing_a_pending_rowspan_keeps_its_slot() {
        let html = r#"<table>
            <thead><tr><th>A</th><th>B</th><th>C</th><th>D</th></tr></thead>
            <tbody>
                <tr><td>a1</td><td>b1</td><td rowspan="2">DDD_ROWSPAN</td><td>d1</td></tr>
                <tr><td colspan="2">wide</td><td>d2</td></tr>
                <tr><td>a3</td><td>b3</td><td>c3</td><td>d3</td></tr>
            </tbody>
        </table>"#;
        let out = normalize_tables(html);
        let doc = Html::parse_document(&out);
        let rows: Vec<Vec<String>> = doc
            .select(&Selector::parse("tbody tr").unwrap())
            .map(|tr| {
                tr.select(&Selector::parse("td").unwrap())
                    .map(|c| c.inner_html())
                    .collect()
            })
            .collect();
        assert_eq!(rows[0], vec!["a1", "b1", "DDD_ROWSPAN", "d1"]);
        assert_eq!(
            rows[1],
            vec!["wide", "", "DDD_ROWSPAN", "d2"],
            "the pending rowspan must claim column 2 in its own row"
        );
        assert_eq!(rows[2], vec!["a3", "b3", "c3", "d3"], "no ghost copy");
    }

    /// C5: colliding rowspans grow the column count linearly with the row
    /// count, so the grid grows quadratically at ANY `MAX_SPAN` value. The
    /// total-cell ceiling is what bounds it; over the ceiling the table is
    /// left untouched.
    #[test]
    fn colliding_rowspans_do_not_blow_up_the_grid() {
        let mut html = String::from("<table><tbody>");
        for r in 0..400 {
            html.push_str(&format!(
                "<tr><td rowspan=\"1000\">g{r}</td><td>v{r}</td></tr>"
            ));
        }
        html.push_str("</tbody></table>");

        let start = std::time::Instant::now();
        let out = normalize_tables(&html);
        let elapsed = start.elapsed();

        assert!(
            out.len() < html.len() * 3,
            "output {} bytes vs input {} bytes, quadratic blow-up",
            out.len(),
            html.len()
        );
        assert!(elapsed.as_millis() < 2_000, "took {elapsed:?}");
        assert!(
            out.contains("g399"),
            "data must survive untouched: {out:.200}"
        );
    }

    /// C2: html5ever auto-closes an unclosed `<table>` and reports two
    /// siblings; `lol_html`'s raw token stream sees the second nested inside
    /// the first. Replacing the first then swallowed the second table's
    /// content. The count mismatch now aborts the splice.
    #[test]
    fn unclosed_table_followed_by_another_loses_no_content() {
        let html = r#"<div>
            <table>
                <thead><tr><th>A</th><th>B</th></tr></thead>
                <tbody><tr><td>first-1</td><td>first-2</td></tr></tbody>
            <table>
                <thead><tr><th>C</th><th>D</th></tr></thead>
                <tbody><tr><td>SECOND_TABLE_DATA</td><td>second-2</td></tr></tbody>
            </table>
        </div>"#;
        let out = normalize_tables(html);
        assert!(
            out.contains("SECOND_TABLE_DATA"),
            "second table swallowed: {out}"
        );
        assert!(out.contains("first-1"), "first table lost: {out}");
    }

    #[test]
    fn rowspan_and_colspan_expand_without_losing_cells() {
        let html = r#"<table>
            <thead><tr><th colspan="2">Name</th><th>Score</th></tr></thead>
            <tbody>
                <tr><td rowspan="2">Alice</td><td>Math</td><td>90</td></tr>
                <tr><td>Physics</td><td>85</td></tr>
            </tbody>
        </table>"#;
        let out = normalize_tables(html);
        let doc = Html::parse_document(&out);
        let tr_sel = Selector::parse("tbody tr").unwrap();
        let cell_sel = Selector::parse("td").unwrap();
        let rows: Vec<Vec<String>> = doc
            .select(&tr_sel)
            .map(|tr| tr.select(&cell_sel).map(|c| c.inner_html()).collect())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0], vec!["Alice", "Math", "90"]);
        assert_eq!(rows[1], vec!["Alice", "Physics", "85"]);
    }

    /// A wide `colspan` is almost always a banner/title row, not repeated data.
    /// Copying its content into every spanned column turned one 850-byte
    /// Wikipedia election banner into a 7,651-byte line and inflated the page
    /// 6.5x. Content belongs in the first slot only.
    #[test]
    fn wide_colspan_content_is_not_duplicated_across_columns() {
        let banner = "BANNER_TEXT";
        let html = format!(
            r#"<table>
            <thead><tr><th>A</th><th>B</th><th>C</th><th>D</th></tr></thead>
            <tbody>
                <tr><td colspan="4">{banner}</td></tr>
                <tr><td>a</td><td>b</td><td>c</td><td>d</td></tr>
            </tbody>
        </table>"#
        );
        let out = normalize_tables(&html);
        assert_eq!(
            out.matches(banner).count(),
            1,
            "banner duplicated across its spanned columns: {out}"
        );
        let doc = Html::parse_document(&out);
        let widths: Vec<usize> = doc
            .select(&Selector::parse("tbody tr").unwrap())
            .map(|tr| tr.select(&Selector::parse("td").unwrap()).count())
            .collect();
        assert_eq!(widths, vec![4, 4], "row must still expand to full width");
    }

    #[test]
    fn degenerate_spans_do_not_panic_or_lose_data() {
        let html = r#"<table>
            <thead><tr><th>A</th><th>B</th></tr></thead>
            <tbody>
                <tr><td rowspan="99">Spans past table end</td><td>1</td></tr>
                <tr><td colspan="0">Zero colspan</td></tr>
                <tr><td>Cell with <table><tr><td>nested</td></tr></table> inside</td><td>3</td></tr>
            </tbody>
        </table>"#;
        // Must not panic; must retain all real cell text somewhere in the output.
        let out = normalize_tables(html);
        assert!(out.contains("Spans past table end"));
        assert!(out.contains("Zero colspan"));
        assert!(out.contains("nested"));
    }

    #[test]
    fn irregular_row_lengths_are_padded() {
        let html = r#"<table>
            <thead><tr><th>A</th><th>B</th><th>C</th></tr></thead>
            <tbody>
                <tr><td>1</td></tr>
                <tr><td>2</td><td>3</td><td>4</td></tr>
            </tbody>
        </table>"#;
        let out = normalize_tables(html);
        let doc = Html::parse_document(&out);
        let tr_sel = Selector::parse("tbody tr").unwrap();
        let cell_sel = Selector::parse("td").unwrap();
        let rows: Vec<usize> = doc
            .select(&tr_sel)
            .map(|tr| tr.select(&cell_sel).count())
            .collect();
        assert_eq!(rows, vec![3, 3], "short row must be padded to full width");
    }

    #[test]
    fn tfoot_preserved_as_trailing_body_rows() {
        let html = r#"<table>
            <thead><tr><th>Item</th><th>Total</th></tr></thead>
            <tbody><tr><td>Widget</td><td>10</td></tr></tbody>
            <tfoot><tr><td>Sum</td><td>10</td></tr></tfoot>
        </table>"#;
        let out = normalize_tables(html);
        let doc = Html::parse_document(&out);
        let tr_sel = Selector::parse("tbody tr").unwrap();
        let rows: Vec<String> = doc.select(&tr_sel).map(|tr| tr.inner_html()).collect();
        assert_eq!(
            rows.len(),
            2,
            "tfoot row must land in tbody as trailing row"
        );
        assert!(rows[1].contains("Sum"));
    }

    #[test]
    fn table_inside_list_item_becomes_real_table_not_fenced_code() {
        let html = r#"<ul><li>Intro text
            <table>
                <thead><tr><th>A</th><th>B</th></tr></thead>
                <tbody><tr><td>1</td><td>2</td></tr></tbody>
            </table>
        </li></ul>"#;
        let md = crate::markdown::html_to_markdown_with(html, true);
        assert!(
            !md.contains("```"),
            "table-in-list must not be fenced: {md}"
        );
        assert!(
            md.contains("| A") || md.contains("|A"),
            "expected a pipe table: {md}"
        );
    }

    #[test]
    fn layout_table_untouched() {
        let html = r#"<table role="presentation">
            <tr><td><img src="logo.png"></td></tr>
            <tr><td>Newsletter content</td></tr>
        </table>"#;
        let out = normalize_tables(html);
        // No thead/th fabricated, no span attributes introduced, structure kept.
        assert!(!out.contains("<thead>"));
        assert!(out.contains("Newsletter content"));
    }

    #[test]
    fn nested_table_lol_html_handler_fires_but_is_a_noop() {
        // Documents the actual lol_html behavior this module depends on:
        // `replace()` on the outer table does NOT suppress the tokenizer from
        // dispatching the `table` element handler for the nested table inside
        // the replaced subtree, hence the depth counter. If lol_html ever
        // changed this, `replacements` indices would desync and this test
        // would start failing loudly instead of silently mis-splicing.
        let html = r#"<table>
            <thead><tr><th>Outer</th></tr></thead>
            <tbody><tr><td>
                <table><tr><td>inner-only, no header</td></tr></table>
            </td></tr></tbody>
        </table>"#;
        let out = normalize_tables(html);
        let doc = Html::parse_document(&out);
        let sel = Selector::parse("table").unwrap();
        let tables: Vec<_> = doc.select(&sel).collect();
        assert_eq!(tables.len(), 2, "outer + nested must both survive: {out}");
        assert!(out.contains("inner-only, no header"));
        // The nested table must stay exactly as authored, no synthesized
        // header, no thead, proving it was never independently replaced.
        let inner_html = tables[1].html();
        assert!(
            !inner_html.contains("<thead>"),
            "nested table must stay a no-op: {inner_html}"
        );
    }

    #[test]
    fn flag_off_is_byte_identical_to_legacy() {
        let html = r#"<table>
            <tr><td scope="row">Revenue</td><td>100</td></tr>
            <tr><td scope="row">Costs</td><td>50</td></tr>
        </table>"#;
        let legacy = crate::markdown::html_to_markdown(html);
        let gated_off = crate::markdown::html_to_markdown_with(html, false);
        assert_eq!(legacy, gated_off);
    }
}
