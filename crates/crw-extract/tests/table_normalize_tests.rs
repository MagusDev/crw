//! Fixture tests for `ExtractionConfig::normalize_tables` (PR 1 spec, §Tests
//! 1-10). Each fixture is checked against BOTH the normalized (`true`) and
//! legacy (`false`) markdown pipelines; every fixture also gets the flag-OFF
//! byte-identical check (#10, the revert guarantee).

use crw_extract::markdown::{html_to_markdown, html_to_markdown_with};

/// #10, applied to every other fixture in this file: flag OFF must be
/// byte-identical to the pre-existing `html_to_markdown` for the same input.
fn assert_flag_off_is_legacy(html: &str) {
    assert_eq!(
        html_to_markdown(html),
        html_to_markdown_with(html, false),
        "flag OFF must be byte-identical to the legacy pipeline"
    );
}

/// Every pipe row in `md`, trimmed.
fn pipe_rows(md: &str) -> Vec<&str> {
    md.lines()
        .map(str::trim)
        .filter(|l| l.starts_with('|'))
        .collect()
}

/// #1: a table with no header markup is not rewritten at all, on or off. Row 0
/// is never promoted to column names: measured across 46 real data tables, the
/// promotion fired once, on a site's layout grid.
#[test]
fn fixture_1_headerless_table_is_identical_on_and_off() {
    let html = r#"<table>
        <tr><td scope="row">Revenue</td><td>100</td><td>200</td></tr>
        <tr><td scope="row">Costs</td><td>50</td><td>60</td></tr>
        <tr><td scope="row">Profit</td><td>50</td><td>140</td></tr>
    </table>"#;
    assert_flag_off_is_legacy(html);
    assert_eq!(
        html_to_markdown_with(html, true),
        html_to_markdown(html),
        "a headerless table must be left exactly as the legacy path renders it"
    );
}

/// #3: `rowspan` + `colspan` expand into a rectangular grid with no lost
/// cells and correct column alignment.
#[test]
fn fixture_3_rowspan_colspan_rectangular_no_lost_cells() {
    let html = r#"<table>
        <thead><tr><th colspan="2">Name</th><th>Score</th></tr></thead>
        <tbody>
            <tr><td rowspan="2">Alice</td><td>Math</td><td>90</td></tr>
            <tr><td>Physics</td><td>85</td></tr>
        </tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    let table_lines: Vec<&str> = md
        .lines()
        .filter(|l| l.trim_start().starts_with('|'))
        .collect();
    // header + separator + 2 body rows
    assert_eq!(table_lines.len(), 4, "expected 4 pipe rows. Got:\n{md}");
    assert!(table_lines[2].contains("Alice") && table_lines[2].contains("Math"));
    assert!(table_lines[3].contains("Alice") && table_lines[3].contains("Physics"));
}

/// #4: degenerate spans (rowspan past table end, colspan="0", a nested table
/// inside a spanned cell) must not panic and must not lose real data.
#[test]
fn fixture_4_degenerate_spans_no_panic_no_data_loss() {
    let html = r#"<table>
        <thead><tr><th>A</th><th>B</th></tr></thead>
        <tbody>
            <tr><td rowspan="99">Spans past table end</td><td>1</td></tr>
            <tr><td colspan="0">Zero colspan</td></tr>
            <tr><td>Cell with <table><tr><td>nested</td></tr></table> inside</td><td>3</td></tr>
        </tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    assert!(md.contains("Spans past table end"));
    assert!(md.contains("Zero colspan"));
    assert!(md.contains("nested"));
}

/// #5: irregular row lengths are padded, not misaligned.
#[test]
fn fixture_5_irregular_row_lengths_padded() {
    let html = r#"<table>
        <thead><tr><th>A</th><th>B</th><th>C</th></tr></thead>
        <tbody>
            <tr><td>1</td></tr>
            <tr><td>2</td><td>3</td><td>4</td></tr>
        </tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    let pipe_counts: Vec<usize> = md
        .lines()
        .filter(|l| l.trim_start().starts_with('|'))
        .map(|l| l.matches('|').count())
        .collect();
    assert!(
        pipe_counts.len() >= 2,
        "expected at least a header + row. Got:\n{md}"
    );
    let first = pipe_counts[0];
    assert!(
        pipe_counts.iter().all(|c| *c == first),
        "all rows must have the same column count. Got: {pipe_counts:?} in:\n{md}"
    );
}

/// #6: `<tfoot>` is preserved as trailing body rows.
#[test]
fn fixture_6_tfoot_preserved_as_trailing_rows() {
    let html = r#"<table>
        <thead><tr><th>Item</th><th>Total</th></tr></thead>
        <tbody><tr><td>Widget</td><td>10</td></tr></tbody>
        <tfoot><tr><td>Sum</td><td>10</td></tr></tfoot>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    assert!(md.contains("Widget"));
    assert!(
        md.contains("Sum"),
        "tfoot row must survive as a body row. Got: {md}"
    );
}

/// #7: a table inside a `<li>` becomes a real pipe table, not a fenced code
/// block (the §Why-3 indented-code bug).
#[test]
fn fixture_7_table_inside_list_item_not_fenced() {
    let html = r#"<ul><li>Intro text
        <table>
            <thead><tr><th>A</th><th>B</th></tr></thead>
            <tbody><tr><td>1</td><td>2</td></tr></tbody>
        </table>
    </li></ul>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    assert!(
        !md.contains("```"),
        "table-in-list must not be fenced. Got: {md}"
    );
    assert!(
        md.contains("| A | B |"),
        "expected a pipe table header. Got: {md}"
    );
    assert!(
        md.contains("| 1 | 2 |"),
        "expected a pipe table row. Got: {md}"
    );
}

/// #8: a layout table (`role="presentation"`, nested, 1-col) is left
/// untouched, same output as today regardless of the flag.
#[test]
fn fixture_8_layout_table_untouched() {
    let html = r#"<table role="presentation">
        <tr><td><img src="logo.png">Header banner<table><tr><td>nested layout</td></tr></table></td></tr>
        <tr><td>Newsletter content</td></tr>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let normalized = html_to_markdown_with(html, true);
    let legacy = html_to_markdown_with(html, false);
    assert_eq!(
        normalized, legacy,
        "a layout table must produce identical output regardless of the flag"
    );
}

/// #9: a nested table is left opaque; the `lol_html` table handler firing for
/// it (see `table_normalize::normalize_tables` doc comment) must not cause it
/// to be independently rewritten or double-processed.
#[test]
fn fixture_9_nested_table_opaque_no_double_processing() {
    let html = r#"<table>
        <thead><tr><th>Outer</th></tr></thead>
        <tbody><tr><td>
            <table><tr><td>inner-only, no header</td></tr></table>
        </td></tr></tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    assert!(
        md.contains("inner-only, no header"),
        "nested content lost. Got: {md}"
    );
}

/// C1: the thead and body grids were rectangularized independently, so a body
/// row wider than every header row lost its overflow cells before htmd ever
/// saw them. No spans involved, plain ragged markup.
#[test]
fn fixture_c1_body_wider_than_header_loses_no_cell() {
    let html = r#"<table>
        <thead><tr><th>Name</th><th>Score</th></tr></thead>
        <tbody>
            <tr><td>Alice</td><td>90</td><td>Bonus note</td></tr>
            <tr><td>Bob</td><td>85</td><td>Second note</td></tr>
        </tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    assert!(
        md.contains("Bonus note"),
        "third body cell dropped. Got: {md}"
    );
    assert!(
        md.contains("Second note"),
        "third body cell dropped. Got: {md}"
    );
    let widths: Vec<usize> = pipe_rows(&md)
        .iter()
        .map(|l| l.matches('|').count())
        .collect();
    assert!(
        widths.windows(2).all(|w| w[0] == w[1]),
        "every row must share the reconciled width. Got: {widths:?} in {md}"
    );
}

/// C2: an unclosed `<table>` immediately followed by another. html5ever
/// auto-closes and reports two siblings; `lol_html` sees the second nested in
/// the first. Splicing on that disagreement swallowed the second table.
#[test]
fn fixture_c2_unclosed_table_soup_loses_no_table() {
    let html = r#"<div>
        <table>
            <thead><tr><th>A</th><th>B</th></tr></thead>
            <tbody><tr><td>first-1</td><td>first-2</td></tr></tbody>
        <table>
            <thead><tr><th>C</th><th>D</th></tr></thead>
            <tbody><tr><td>SECONDTABLEDATA</td><td>second-2</td></tr></tbody>
        </table>
    </div>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    assert!(md.contains("first-1"), "first table lost. Got: {md}");
    assert!(
        md.contains("SECONDTABLEDATA"),
        "second table swallowed. Got: {md}"
    );
}

/// C3: a `colspan` walking across a pending `rowspan`'s column used to consume
/// that column, so the spanned value disappeared from its own row and
/// reappeared as a ghost copy one row later.
#[test]
fn fixture_c3_colspan_crossing_pending_rowspan() {
    let html = r#"<table>
        <thead><tr><th>A</th><th>B</th><th>C</th><th>D</th></tr></thead>
        <tbody>
            <tr><td>a1</td><td>b1</td><td rowspan="2">DDDSPAN</td><td>d1</td></tr>
            <tr><td colspan="2">wide</td><td>d2</td></tr>
            <tr><td>a3</td><td>b3</td><td>c3</td><td>d3</td></tr>
        </tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    // header, separator, then the three body rows.
    let rows = pipe_rows(&md);
    assert_eq!(rows.len(), 5, "Got: {md}");
    assert!(
        rows[2].contains("DDDSPAN"),
        "row 1 lost the span. Got: {md}"
    );
    assert!(
        rows[3].contains("DDDSPAN") && rows[3].contains("wide"),
        "the colspan must not consume the pending span's column. Got: {md}"
    );
    assert!(
        !rows[4].contains("DDDSPAN"),
        "ghost copy leaked into row 3. Got: {md}"
    );
}

/// C5: colliding `rowspan`s make the column count grow with the row count, so
/// the grid grows quadratically even AT the `MAX_SPAN` clamp. Output size must
/// stay roughly linear in input size.
#[test]
fn fixture_c5_colliding_rowspans_stay_linear() {
    let mut html = String::from("<table><tbody>");
    for r in 0..400 {
        html.push_str(&format!(
            "<tr><td rowspan=\"1000\">group{r}</td><td>value{r}</td></tr>"
        ));
    }
    html.push_str("</tbody></table>");

    let md = html_to_markdown_with(&html, true);
    assert!(
        md.len() < html.len() * 3,
        "markdown {} bytes from {} bytes of html, quadratic blow-up",
        md.len(),
        html.len()
    );
    assert!(md.contains("group399"), "data lost. Got {} bytes", md.len());
}

/// A multi-row header where row 0 is colspan'd group labels. Each sub-column
/// must keep its group ("Group A / Sub2"), while a FULL-width banner row is
/// still emitted once rather than repeated across every column.
#[test]
fn fixture_c7_colspan_group_header_labels_every_subcolumn() {
    let html = r#"<table>
        <thead>
            <tr><th colspan="5">BANNERROW</th></tr>
            <tr><th colspan="3">Group A</th><th colspan="2">Group B</th></tr>
            <tr><th>Sub1</th><th>Sub2</th><th>Sub3</th><th>Sub4</th><th>Sub5</th></tr>
        </thead>
        <tbody><tr><td>1</td><td>2</td><td>3</td><td>4</td><td>5</td></tr></tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    let header = pipe_rows(&md)[0];
    for sub in ["Sub1", "Sub2", "Sub3"] {
        assert!(
            header.contains(&format!("Group A / {sub}")),
            "{sub} lost its group label. Got: {header}"
        );
    }
    for sub in ["Sub4", "Sub5"] {
        assert!(
            header.contains(&format!("Group B / {sub}")),
            "{sub} lost its group label. Got: {header}"
        );
    }
    assert_eq!(
        md.matches("BANNERROW").count(),
        1,
        "a full-width banner must not repeat across columns. Got: {md}"
    );
}

/// A leading `rowspan` column makes the header rows narrower than the table, so
/// a row's own colspans understate the true width. A `colspan="3"` group in row
/// 2 of a 4-wide table used to look full-width, get treated as a banner, and
/// drop its label from every sub-column but the first.
#[test]
fn fixture_c8_rowspan_column_does_not_hide_the_true_width() {
    let html = r#"<table>
        <thead>
            <tr><th rowspan="2">Rank</th><th colspan="3">Player Stats</th></tr>
            <tr><th>Team</th><th>Goals</th><th>Assists</th></tr>
        </thead>
        <tbody><tr><td>1</td><td>Ajax</td><td>12</td><td>7</td></tr></tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let header = html_to_markdown_with(html, true);
    let header = pipe_rows(&header)[0].to_string();
    for sub in ["Team", "Goals", "Assists"] {
        assert!(
            header.contains(&format!("Player Stats / {sub}")),
            "{sub} lost its group label, true width was misread. Got: {header}"
        );
    }
}

/// A merged DATA cell is one value covering a range, not a value each column
/// holds. Copying "240" into both Q1 and Q2 would state that each quarter was
/// 240, which the page never said. Only a `<th>` group label repeats.
#[test]
fn fixture_c9_merged_data_cell_is_not_copied_across_columns() {
    let html = r#"<table>
        <thead><tr><th>Region</th><th>Q1</th><th>Q2</th></tr></thead>
        <tbody>
            <tr><td>North</td><td colspan="2">240 combined</td></tr>
            <tr><td>South</td><td>50</td><td>60</td></tr>
        </tbody>
    </table>"#;
    assert_flag_off_is_legacy(html);

    let md = html_to_markdown_with(html, true);
    assert_eq!(
        md.matches("240 combined").count(),
        1,
        "a merged datum must not be duplicated per column. Got: {md}"
    );
    assert!(md.contains("North"), "row lost. Got: {md}");
    assert!(
        md.contains("50") && md.contains("60"),
        "row lost. Got: {md}"
    );
}
