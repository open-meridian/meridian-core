//! The figures a plugin reports on its heartbeat (W4.5), and their bounds.
//!
//! A short list about the plugin's own work -- Connections, Accounts reached,
//! Last read -- which core draws as tiles on the plugin's Summary under Manage
//! (W6.9). The sidecar carries the last accepted list on the plugin's report
//! (W4.8).
//!
//! Bounded, and refused rather than cut: a heartbeat breaking any bound is
//! refused whole, INVALID_ARGUMENT, naming the figure, the field and the bound
//! in the words meridian-design's fixtures/sidecar/heartbeat.yaml pins. The
//! SDK refuses the same in the plugin's process; this is for a plugin that
//! built its heartbeat by hand. Each bound is the data dictionary's entry, as
//! `meridian_pb::bounds` generates it, and none is written here.

use std::collections::BTreeSet;

use meridian_domain::exact::Exact;
use meridian_pb::bounds::{
    Length, HEARTBEAT_REQUEST_FIGURES_COUNT as FIGURES, PLUGIN_FIGURE_LABEL_LENGTH as LABEL,
    PLUGIN_FIGURE_TEXT_LENGTH as TEXT, PLUGIN_FIGURE_WHY_LENGTH as WHY,
};
use meridian_pb::v1::plugin_figure::Value;
use meridian_pb::v1::{FigureState, PluginFigure};

/// The figures as given, or the refusal naming what broke which bound.
pub fn check(figures: &[PluginFigure]) -> Result<(), String> {
    if !FIGURES.admits(figures.len()) {
        return Err(format!(
            "{} figures; a plugin reports at most {}",
            figures.len(),
            FIGURES.most
        ));
    }
    let mut labels = BTreeSet::new();
    for (i, figure) in figures.iter().enumerate() {
        let at = format!("figures[{i}]");
        if figure.label.is_empty() {
            return Err(format!(
                "{at}.label is empty; a label is {} to {} characters",
                LABEL.least, LABEL.most
            ));
        }
        within(&at, "label", "a label", &figure.label, LABEL)?;
        if !labels.insert(figure.label.as_str()) {
            return Err(format!(
                "{at}.label {:?} is given twice; a label is given once",
                figure.label
            ));
        }
        match &figure.value {
            None => {
                return Err(format!(
                    "{at} has no value; a figure is a count, a decimal, a text or a time"
                ))
            }
            Some(Value::Text(text)) => within(&at, "text", "a text", text, TEXT)?,
            Some(Value::Decimal(decimal)) => {
                Exact::from_wire(decimal).map_err(|out_of_range| {
                    format!("{at}.decimal {out_of_range}; it is refused rather than rounded")
                })?;
            }
            Some(Value::Count(_) | Value::AtNs(_)) => {}
        }
        if FigureState::try_from(figure.state).is_err() {
            return Err(format!(
                "{at}.state is {}, which the contract does not define",
                figure.state
            ));
        }
        within(&at, "why", "a why", &figure.why, WHY)?;
    }
    Ok(())
}

/// A text field no longer than its bound, counted in characters as a person
/// reads them rather than in bytes.
fn within(at: &str, field: &str, what: &str, text: &str, bound: Length) -> Result<(), String> {
    let length = text.chars().count();
    if length > bound.most {
        return Err(format!(
            "{at}.{field} is {length} characters; {what} is at most {}",
            bound.most
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use meridian_pb::v1::Decimal;

    use super::*;

    fn count(label: &str, count: i64) -> PluginFigure {
        PluginFigure {
            label: label.into(),
            value: Some(Value::Count(count)),
            ..Default::default()
        }
    }

    /// SnapTrade's, then a decimal and a text: the heartbeat fixture's list.
    fn snaptrade() -> Vec<PluginFigure> {
        vec![
            PluginFigure {
                state: FigureState::Warn as i32,
                why: "1 connection needs attention: the brokerage asked to reconnect".into(),
                ..count("Connections", 3)
            },
            count("Accounts reached", 7),
            PluginFigure {
                label: "Last read".into(),
                value: Some(Value::AtNs(1_790_380_500_000_000_000)),
                ..Default::default()
            },
            PluginFigure {
                label: "Rows refused".into(),
                value: Some(Value::Decimal(Decimal {
                    high: 0,
                    low: 5,
                    scale: 1,
                })),
                why: "per statement, over the last day".into(),
                ..Default::default()
            },
            PluginFigure {
                label: "Key".into(),
                value: Some(Value::Text("Commercial".into())),
                as_of_ns: 1_790_380_000_000_000_000,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn the_fixtures_figures_and_none_are_accepted() {
        assert_eq!(check(&snaptrade()), Ok(()));
        assert_eq!(check(&[]), Ok(()));
        let eight: Vec<_> = (0..8).map(|i| count(&format!("F{i}"), i)).collect();
        assert_eq!(check(&eight), Ok(()));
    }

    // The refusals are the fixture's, word for word.

    #[test]
    fn more_than_eight_is_refused_never_cut() {
        let nine: Vec<_> = (0..9).map(|i| count(&format!("F{i}"), i)).collect();
        assert_eq!(
            check(&nine),
            Err("9 figures; a plugin reports at most 8".into())
        );
    }

    #[test]
    fn a_label_over_forty_characters_is_refused() {
        let long = count("Connections that need the admin to reconnect", 1);
        assert_eq!(
            check(&[long]),
            Err("figures[0].label is 44 characters; a label is at most 40".into())
        );
        // Characters, not bytes: forty accented letters are forty.
        assert_eq!(check(&[count(&"é".repeat(40), 1)]), Ok(()));
    }

    #[test]
    fn a_text_or_a_why_past_its_bound_is_refused_naming_the_field() {
        let text = PluginFigure {
            label: "Key".into(),
            value: Some(Value::Text("x".repeat(41))),
            ..Default::default()
        };
        assert_eq!(
            check(&[text]),
            Err("figures[0].text is 41 characters; a text is at most 40".into())
        );
        let why = PluginFigure {
            why: "x".repeat(201),
            ..count("Connections", 3)
        };
        assert_eq!(
            check(&[count("Accounts", 1), why]),
            Err("figures[1].why is 201 characters; a why is at most 200".into())
        );
    }

    #[test]
    fn a_state_the_contract_does_not_define_is_refused() {
        let undefined = PluginFigure {
            state: 7,
            ..count("Connections", 3)
        };
        assert_eq!(
            check(&[undefined]),
            Err("figures[0].state is 7, which the contract does not define".into())
        );
    }

    #[test]
    fn a_figure_with_no_value_is_refused() {
        let none = PluginFigure {
            label: "Connections".into(),
            ..Default::default()
        };
        assert_eq!(
            check(&[none]),
            Err("figures[0] has no value; a figure is a count, a decimal, a text or a time".into())
        );
    }

    #[test]
    fn an_empty_or_repeated_label_is_refused_naming_the_figure() {
        assert_eq!(
            check(&[count("", 1)]),
            Err("figures[0].label is empty; a label is 1 to 40 characters".into())
        );
        assert_eq!(
            check(&[count("Connections", 1), count("Connections", 2)]),
            Err("figures[1].label \"Connections\" is given twice; a label is given once".into())
        );
    }

    #[test]
    fn a_decimal_outside_its_range_is_refused_as_every_decimal_is() {
        let too_fine = PluginFigure {
            label: "Rows refused".into(),
            value: Some(Value::Decimal(Decimal {
                high: 0,
                low: 1,
                scale: 19,
            })),
            ..Default::default()
        };
        assert_eq!(
            check(&[too_fine]),
            Err(
                "figures[0].decimal has 19 decimal places, and at most 18 cross the wire; \
                 it is refused rather than rounded"
                    .into()
            )
        );
    }
}
