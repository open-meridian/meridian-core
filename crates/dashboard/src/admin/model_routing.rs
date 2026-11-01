//! The analytics routing table, on its Settings (contract v26, W12.6; the
//! one-API spec's requirements 21, 22 and 34; ruling 285): a deployment
//! admin's, each model id on one line -- its implementation, `built-in`
//! unless routed to a launched `pricing` instance, and since when and by
//! whom -- with Change, a reason required; each change its own record,
//! kept by the conductor. Under parity `dashboard__list_model_routing` and
//! `dashboard__set_model_routing`.

use std::time::Duration;

use meridian_domain::v1::{
    ListMeasureDefinitionsReply, ListMeasureDefinitionsRequest, ListModelRoutingReply,
    ListModelRoutingRequest,
};
use prost::Message;
use serde_json::{json, Value};

use crate::html::escape;

pub const SET_MODEL_ROUTING: &str = "platform.analytics.command.set-model-routing";
pub const LIST_MODEL_ROUTING: &str = "platform.analytics.query.list-model-routing";
pub const LIST_MEASURE_DEFINITIONS: &str = "platform.analytics.query.list-measure-definitions";

/// Where the tab's form posts.
pub const PATH: &str = "/admin/model-routing";

/// The implementation every model id has.
pub const BUILT_IN: &str = "built-in";

/// The model ids the conductor routes (W12.6): v26's, v28's option models
/// and calibrations (565), v29's debt models and curve (688), as the
/// conductor's table lists them.
pub const MODEL_IDS: [&str; 11] = [
    "cash",
    "quoted_price",
    "binary_event",
    "american_binomial",
    "black_scholes",
    "black_76",
    "svi",
    "ssvi",
    "bond_cash_flows",
    "bond_yield_solver",
    "monotone_convex",
];

/// The routing table in force, from the conductor.
pub async fn read(bus: &meridian_bus::Bus) -> Result<ListModelRoutingReply, String> {
    let (_, bytes) = bus
        .call(
            LIST_MODEL_ROUTING,
            "meridian.v1.ListModelRoutingRequest",
            ListModelRoutingRequest {}.encode_to_vec(),
            None,
            Some(Duration::from_secs(5)),
        )
        .await
        .map_err(|failed| failed.to_string())?;
    ListModelRoutingReply::decode(&bytes[..])
        .map_err(|failed| format!("the conductor's answer did not read: {failed}"))
}

/// The measure definitions the analytics component serves, where it
/// answers: named beside the models.
pub async fn definitions(bus: &meridian_bus::Bus) -> Option<ListMeasureDefinitionsReply> {
    let (_, bytes) = bus
        .call(
            LIST_MEASURE_DEFINITIONS,
            "meridian.v1.ListMeasureDefinitionsRequest",
            ListMeasureDefinitionsRequest {}.encode_to_vec(),
            None,
            Some(Duration::from_secs(3)),
        )
        .await
        .ok()?;
    ListMeasureDefinitionsReply::decode(&bytes[..]).ok()
}

/// The table as a tool answers it, with the measures the analytics
/// component serves beside it, as the tab's "Measures served" line reads
/// them (MCP parity; null where the component does not answer).
pub fn json(table: &ListModelRoutingReply, served: Option<&ListMeasureDefinitionsReply>) -> Value {
    json!({
        "version": table.version.to_string(),
        "routes": table.routes.iter().map(|r| json!({
            "model_id": r.model_id,
            "implementation": r.implementation,
            "changed_at_ns": r.changed_at_ns.to_string(),
            "changed_by": r.changed_by,
            "reason": r.reason,
        })).collect::<Vec<_>>(),
        "measures_served": served.map(|served| served.definitions.iter().map(|d| json!({
            "definition": d.definition,
            "version": d.version,
        })).collect::<Vec<_>>()),
    })
}

/// The tab's lines: each model id on one line, then the definitions served.
pub fn lines(
    table: Option<&ListModelRoutingReply>,
    definitions: Option<&ListMeasureDefinitionsReply>,
    name_of: impl Fn(&str) -> String,
) -> String {
    let Some(table) = table else {
        return "<p class=\"warn\">The routing table could not be read just now.</p>".to_string();
    };
    let rows: String = table
        .routes
        .iter()
        .map(|route| {
            let since = if route.changed_at_ns == 0 {
                "never routed".to_string()
            } else {
                format!(
                    "since {} by {}",
                    escape(&crate::custody::utc(route.changed_at_ns)),
                    escape(&name_of(&route.changed_by))
                )
            };
            let reason = if route.reason.is_empty() {
                String::new()
            } else {
                format!(" title=\"{}\"", escape(&route.reason))
            };
            format!(
                "<dt data-model-id=\"{model}\"><code>{model}</code></dt><dd{reason}>\
                 <strong>{implementation}</strong> <span class=\"faint\">{since}</span></dd>",
                model = escape(&route.model_id),
                implementation = escape(&route.implementation),
            )
        })
        .collect();
    let served = match definitions {
        Some(served) if !served.definitions.is_empty() => format!(
            "<p class=\"hint\" data-definitions>Measures served: {}.</p>",
            served
                .definitions
                .iter()
                .map(|d| format!("{} v{}", escape(&d.definition), d.version))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => "<p class=\"hint faint\">The analytics component is not answering: the measures it \
              serves are not listed.</p>"
            .to_string(),
    };
    format!(
        "<dl class=\"facts\" data-routing-version=\"{}\">{rows}</dl>{served}",
        table.version
    )
}

/// The dialog's fields: the model id, its implementation, and why.
pub fn fields(table: Option<&ListModelRoutingReply>) -> String {
    let models: String = table
        .map(|t| t.routes.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|r| {
            format!(
                "<option value=\"{m}\">{m}</option>",
                m = escape(&r.model_id)
            )
        })
        .collect();
    format!(
        "<label>Model id<select name=\"model_id\" required>{models}</select></label>\
         <label>Implementation<input name=\"implementation\" value=\"{BUILT_IN}\" required \
         maxlength=\"128\" autocomplete=\"off\" spellcheck=\"false\"></label>\
         <p class=\"hint\">built-in, or the instance ID of a launched pricing plugin. A routed \
         plugin that does not answer leaves the figures through it not evaluated, never \
         answered by the built-in model in silence.</p>\
         <label>Why<textarea name=\"note\" required maxlength=\"2000\" rows=\"2\"></textarea></label>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use meridian_domain::v1::{MeasureDefinition, ModelRouting};

    #[test]
    fn each_model_id_is_one_line_with_its_implementation_and_since_when() {
        let table = ListModelRoutingReply {
            routes: vec![
                ModelRouting {
                    model_id: "cash".into(),
                    implementation: BUILT_IN.into(),
                    ..Default::default()
                },
                ModelRouting {
                    model_id: "quoted_price".into(),
                    implementation: "sample-pricing-1".into(),
                    changed_at_ns: 1_792_300_000_000_000_000,
                    changed_by: "local|ada".into(),
                    reason: "Prove the pricing seam".into(),
                },
            ],
            version: 2,
        };
        let served = ListMeasureDefinitionsReply {
            definitions: vec![MeasureDefinition {
                definition: "historical_var".into(),
                version: 1,
                ..Default::default()
            }],
        };
        let said = lines(Some(&table), Some(&served), |_| "Ada".into());
        assert_eq!(said.matches("<dt data-model-id").count(), 2);
        assert!(said.contains("<strong>sample-pricing-1</strong>"), "{said}");
        assert!(
            said.contains("by Ada") && said.contains("never routed"),
            "{said}"
        );
        assert!(said.contains("title=\"Prove the pricing seam\""), "{said}");
        assert!(said.contains("historical_var v1"), "{said}");
        assert!(lines(Some(&table), None, |s| s.to_string()).contains("not answering"));
        assert!(lines(None, None, |s| s.to_string()).contains("could not be read"));
        assert_eq!(json(&table, None)["version"], "2");
        // MCP parity with the tab's Measures served line.
        assert!(json(&table, None)["measures_served"].is_null());
        assert_eq!(
            json(&table, Some(&served))["measures_served"][0]["definition"],
            "historical_var"
        );
        assert!(fields(Some(&table)).contains("<option value=\"quoted_price\">"));
        // A figure is a figure: nothing here speaks of compliance.
        assert!(!said.to_ascii_lowercase().contains("complian"));
    }
}
