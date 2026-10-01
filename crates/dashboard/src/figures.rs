//! The figures a plugin reports, drawn as tiles on its Summary under Manage
//! (W4.5, W4.8, W6.9; sdk-contract/a-plugin-reports-its-figures).
//!
//! Below core's own status, in the order the plugin gave them: each tile its
//! label, its value written by its kind -- a count, a decimal exactly as it
//! was stated, a text, a time as this dashboard shows every moment -- and,
//! where given, when it was true. A figure's state is the tile's mark, the
//! status dot the area's heading draws, and its why the note in the one
//! bubble every note on the page is shown in; without script the why is a
//! line of the tile. Every word is the plugin's, so all of it is escaped.
//!
//! None are drawn for a plugin that is not registered, or whose sidecar has
//! fallen silent: those figures would be stale, and its status already says
//! why there are none.

use meridian_domain::exact::Exact;
use meridian_domain::v1::PluginReport;
use meridian_pb::v1::plugin_figure::Value;
use meridian_pb::v1::{FigureState, PluginFigure};

use crate::custody::utc;
use crate::health::SILENT_NS;
use crate::html::escape;

/// The id of the place on Summary the tiles are drawn in.
pub(crate) const FIGURES: &str = "figures";

/// The place on Summary, holding a tile for each figure the plugin last
/// reported, or empty -- and then taking no room -- when there are none to
/// show.
pub(crate) fn section(report: Option<&PluginReport>, now: i64) -> String {
    let shown =
        report.filter(|report| report.registered && now - report.reported_at_ns <= SILENT_NS);
    let tiles: String = shown
        .map(|report| {
            report
                .figures
                .iter()
                .enumerate()
                .filter_map(|(i, figure)| tile(i, figure))
                .collect()
        })
        .unwrap_or_default();
    if tiles.is_empty() {
        return format!("<section class=\"figures\" id=\"{FIGURES}\"></section>");
    }
    format!(
        "<section class=\"figures\" id=\"{FIGURES}\" aria-label=\"What the plugin reports\">{tiles}</section>"
    )
}

/// One figure's tile, or none for one with no value, which its sidecar
/// refuses and so never reports.
fn tile(i: usize, figure: &PluginFigure) -> Option<String> {
    let (kind, value) = match figure.value.as_ref()? {
        Value::Count(count) => ("count", count.to_string()),
        Value::Decimal(decimal) => (
            "decimal",
            Exact::from_wire(decimal)
                .map(|exact| exact.to_string())
                .unwrap_or_else(|_| "out of range".into()),
        ),
        Value::Text(text) => ("text", text.clone()),
        Value::AtNs(at) => ("time", utc(*at)),
    };
    let label = escape(&figure.label);
    let why_id = format!("{FIGURES}-{i}-why");
    let has_why = !figure.why.trim().is_empty();
    let described = if has_why {
        format!(" aria-describedby=\"{why_id}\"")
    } else {
        String::new()
    };
    let state = match FigureState::try_from(figure.state) {
        Ok(FigureState::Ok) => Some(("ok", "OK")),
        Ok(FigureState::Warn) => Some(("warn", "Needs attention")),
        Ok(FigureState::Error) => Some(("error", "Error")),
        _ => None,
    };
    // The state is the tile's mark, carrying the why as its note; with no
    // state, a why still has its mark to be read from.
    let mark = match state {
        Some((state, word)) => format!(
            "<button type=\"button\" class=\"status-dot\" data-state=\"{state}\" \
             aria-label=\"{word}\" data-note=\"{word}\"{described}></button>"
        ),
        None if has_why => format!(
            "<button type=\"button\" class=\"note-mark\" aria-label=\"Why\" data-note{described}></button>"
        ),
        None => String::new(),
    };
    let as_of = if figure.as_of_ns > 0 {
        format!(
            "<p class=\"figure-as-of\">As of {}</p>",
            escape(&utc(figure.as_of_ns))
        )
    } else {
        String::new()
    };
    let why = if has_why {
        format!(
            "<span class=\"hint noted\" id=\"{why_id}\">{}</span>",
            escape(&figure.why)
        )
    } else {
        String::new()
    };
    let state_attr = state
        .map(|(state, _)| format!(" data-state=\"{state}\""))
        .unwrap_or_default();
    Some(format!(
        "<div class=\"figure\" data-figure=\"{label}\" data-kind=\"{kind}\"{state_attr}>\
         <div class=\"figure-head\"><span class=\"figure-label\">{label}</span>{mark}</div>\
         <p class=\"figure-value\">{}</p>{as_of}{why}</div>",
        escape(&value)
    ))
}

#[cfg(test)]
mod tests {
    use meridian_pb::v1::Decimal;

    use super::*;

    const T0: i64 = 1_790_380_800_000_000_000;

    fn figure(label: &str, value: Value) -> PluginFigure {
        PluginFigure {
            label: label.into(),
            value: Some(value),
            ..Default::default()
        }
    }

    /// The plugin-report fixture's figures, then a decimal and a text.
    fn snaptrade() -> PluginReport {
        PluginReport {
            plugin_instance_id: "snaptrade-1".into(),
            registered: true,
            healthy: true,
            reported_at_ns: T0,
            figures: vec![
                PluginFigure {
                    state: FigureState::Warn as i32,
                    why: "1 connection needs attention: the brokerage asked to reconnect".into(),
                    ..figure("Connections", Value::Count(3))
                },
                figure("Accounts reached", Value::Count(7)),
                figure("Last read", Value::AtNs(1_790_380_500_000_000_000)),
                PluginFigure {
                    why: "per statement, over the last day".into(),
                    ..figure(
                        "Rows refused",
                        Value::Decimal(Decimal {
                            high: 0,
                            low: 50,
                            scale: 2,
                        }),
                    )
                },
                PluginFigure {
                    as_of_ns: 1_790_380_000_000_000_000,
                    ..figure("Key", Value::Text("<Commercial>".into()))
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn each_figure_is_a_tile_in_the_plugins_order_its_value_written_by_its_kind() {
        let drawn = section(Some(&snaptrade()), T0);
        let order: Vec<usize> = [
            "Connections",
            "Accounts reached",
            "Last read",
            "Rows refused",
            "Key",
        ]
        .iter()
        .map(|label| {
            drawn
                .find(&format!("data-figure=\"{label}\""))
                .expect(label)
        })
        .collect();
        assert!(order.windows(2).all(|pair| pair[0] < pair[1]), "{drawn}");
        assert!(drawn.contains(
            "<div class=\"figure\" data-figure=\"Accounts reached\" data-kind=\"count\">\
             <div class=\"figure-head\"><span class=\"figure-label\">Accounts reached</span></div>\
             <p class=\"figure-value\">7</p></div>"
        ));
        // A time as every moment here is shown; a decimal exactly as stated.
        assert!(drawn.contains("<p class=\"figure-value\">2026-09-25 23:55 UTC</p>"));
        assert!(drawn.contains("data-kind=\"decimal\""));
        assert!(drawn.contains("<p class=\"figure-value\">0.50</p>"));
        // The plugin's words are text, never markup.
        assert!(drawn.contains("<p class=\"figure-value\">&lt;Commercial&gt;</p>"));
        assert!(drawn.contains("<p class=\"figure-as-of\">As of 2026-09-25 23:46 UTC</p>"));
    }

    #[test]
    fn a_state_is_the_tiles_mark_and_a_why_its_note() {
        let drawn = section(Some(&snaptrade()), T0);
        assert!(drawn.contains(
            "<div class=\"figure\" data-figure=\"Connections\" data-kind=\"count\" data-state=\"warn\">\
             <div class=\"figure-head\"><span class=\"figure-label\">Connections</span>\
             <button type=\"button\" class=\"status-dot\" data-state=\"warn\" aria-label=\"Needs attention\" \
             data-note=\"Needs attention\" aria-describedby=\"figures-0-why\"></button></div>\
             <p class=\"figure-value\">3</p>\
             <span class=\"hint noted\" id=\"figures-0-why\">1 connection needs attention: the brokerage \
             asked to reconnect</span></div>"
        ));
        // A why with no state still has a mark to be read from.
        assert!(drawn.contains(
            "<button type=\"button\" class=\"note-mark\" aria-label=\"Why\" data-note \
             aria-describedby=\"figures-3-why\"></button>"
        ));
        assert_eq!(
            drawn.matches("<button").count(),
            2,
            "no mark without a state or a why"
        );
    }

    #[test]
    fn none_are_drawn_for_a_plugin_not_registered_silent_or_reporting_none() {
        let empty = "<section class=\"figures\" id=\"figures\"></section>";
        assert_eq!(section(None, T0), empty);
        let left = PluginReport {
            registered: false,
            ..snaptrade()
        };
        assert_eq!(section(Some(&left), T0), empty);
        assert_eq!(section(Some(&snaptrade()), T0 + SILENT_NS + 1), empty);
        let none = PluginReport {
            figures: vec![],
            ..snaptrade()
        };
        assert_eq!(section(Some(&none), T0), empty);
        assert!(section(Some(&snaptrade()), T0 + SILENT_NS).contains("class=\"figure\""));
    }
}
