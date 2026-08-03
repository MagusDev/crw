//! Regression tests for `ExtractionConfig::normalize_tables` beyond the
//! fixture set.

use crw_extract::markdown::html_to_markdown_with;
use crw_extract::quality;

fn table_bearing_pages() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "financial_report",
            r#"<article>
                <h1>Q3 Earnings Report</h1>
                <p>Quarterly revenue grew across all three regional segments, driven by continued
                strength in enterprise subscriptions and a rebound in hardware demand following the
                prior quarter's channel inventory correction.</p>
                <table>
                    <thead><tr><th>Segment</th><th>Q1</th><th>Q2</th><th>Q3</th></tr></thead>
                    <tbody>
                        <tr><td>Cloud</td><td>120</td><td>135</td><td>150</td></tr>
                        <tr><td>Hardware</td><td>80</td><td>70</td><td>95</td></tr>
                        <tr><td>Services</td><td>40</td><td>44</td><td>48</td></tr>
                    </tbody>
                </table>
                <p>Management reaffirmed full-year guidance, citing durable demand in the cloud
                segment and an improving order backlog for hardware shipments in the fourth quarter.</p>
            </article>"#,
        ),
        (
            "sports_stats",
            r#"<article>
                <h1>Season Standings</h1>
                <p>With three games remaining in the regular season, the top four playoff spots
                remain contested among five teams separated by a single game in the standings.</p>
                <table role="table">
                    <tr><td scope="row">Falcons</td><td>82</td><td>45</td><td>0.646</td></tr>
                    <tr><td scope="row">Ravens</td><td>79</td><td>48</td><td>0.622</td></tr>
                    <tr><td scope="row">Wolves</td><td>76</td><td>51</td><td>0.598</td></tr>
                    <tr><td scope="row">Bears</td><td>74</td><td>53</td><td>0.583</td></tr>
                </table>
                <p>The final week's schedule favors the Falcons, who face two of the league's
                weakest offenses before closing the season against a depleted divisional rival.</p>
            </article>"#,
        ),
        (
            "product_spec",
            r#"<article>
                <h1>Product Comparison</h1>
                <p>The updated lineup narrows the gap between the entry and mid-tier models while
                keeping the flagship's premium materials and extended battery life intact.</p>
                <table>
                    <thead><tr><th>Model</th><th>Battery</th><th>Weight</th></tr></thead>
                    <tbody>
                        <tr><td>Entry</td><td rowspan="2">18h</td><td>310g</td></tr>
                        <tr><td>Mid</td><td>295g</td></tr>
                        <tr><td>Flagship</td><td>22h</td><td>340g</td></tr>
                    </tbody>
                </table>
                <p>Pricing for the mid-tier model undercuts last year's flagship by a wide margin,
                making it the likely volume driver for the upcoming holiday shopping season.</p>
            </article>"#,
        ),
    ]
}

/// Alternates-ladder no-flip. Reproduces the specific gating decision at
/// `crw-extract/src/lib.rs:572-653` — primary quality score vs. the 0.4
/// skip-alternates threshold — using the real `quality::analyze_md_only`.
/// Table normalization changes the primary candidate's markdown bytes; this
/// asserts it does not flip which side of the threshold a table-bearing page
/// lands on for this corpus. Any flip is printed (not silently swallowed) so
/// a real regression is investigated rather than papered over.
#[test]
fn alternates_ladder_primary_gate_does_not_flip() {
    const PRIMARY_THRESHOLD: f32 = 0.4;
    let mut flips = Vec::new();

    for (name, html) in table_bearing_pages() {
        let primary_off = html_to_markdown_with(html, false);
        let primary_on = html_to_markdown_with(html, true);
        let score_off = quality::analyze_md_only(&primary_off).score;
        let score_on = quality::analyze_md_only(&primary_on).score;
        let decision_off = score_off > PRIMARY_THRESHOLD;
        let decision_on = score_on > PRIMARY_THRESHOLD;

        if decision_off != decision_on {
            flips.push(format!(
                "{name}: OFF score={score_off:.3} (skip_alternates={decision_off}) vs \
                 ON score={score_on:.3} (skip_alternates={decision_on})"
            ));
        }
    }

    assert!(
        flips.is_empty(),
        "alternates-ladder primary-gate decision flipped for: {flips:#?}"
    );
}
