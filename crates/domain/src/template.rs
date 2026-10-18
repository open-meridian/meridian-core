//! A template version's calculation package, read from the platform's JSON
//! (contract v26, W1.24, W3.24; the intent's rulings 9 and 11; the v26
//! plan's question 225).
//!
//! The package is declarative data: the pricing model the version binds and
//! its version, the kinds of market object it reads, where its corporate
//! action factors come from, its calculation version -- which every
//! valuation names -- and test cases whose inputs and exact answers are
//! canonical JSON. The engine ships the implementation; the package never
//! carries code. The instrument store keeps a template whole, as the bytes
//! of its message, so the package rides with it and is read back unchanged.

use crate::v1::{CalculationPackage, CalculationTestCase};

/// The models v26's packages bind (W12.3): the analytics engine's built-in
/// three. A package naming another is refused rather than kept, since no
/// implementation exists to equal its test cases.
pub const MODELS: [&str; 3] = ["cash", "quoted_price", "binary_event"];

/// The package `value` spells, or `None` where the template carries none --
/// a class's base template, and every version published before v26 -- or the
/// refusal, in words, naming the field.
pub fn package_from_json(
    value: Option<&serde_json::Value>,
) -> Result<Option<CalculationPackage>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or("calculation_package is an object")?;
    let text = |name: &str| -> String {
        match object.get(name) {
            Some(serde_json::Value::String(text)) => text.clone(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            _ => String::new(),
        }
    };
    let model_version = match object.get("model_version") {
        Some(serde_json::Value::Number(number)) => number.as_i64().unwrap_or(-1),
        Some(serde_json::Value::String(text)) => text.trim().parse().unwrap_or(-1),
        None => 0,
        _ => -1,
    };
    let strings = |name: &str| -> Vec<String> {
        object
            .get(name)
            .and_then(|list| list.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let test_cases = object
        .get("test_cases")
        .and_then(|cases| cases.as_array())
        .map(|cases| {
            cases
                .iter()
                .map(|case| {
                    let field = |name: &str| {
                        case.get(name)
                            .and_then(|text| text.as_str())
                            .unwrap_or_default()
                            .to_string()
                    };
                    CalculationTestCase {
                        name: field("name"),
                        inputs: field("inputs"),
                        expected: field("expected"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let package = CalculationPackage {
        model_id: text("model_id"),
        model_version: i32::try_from(model_version).unwrap_or(-1),
        pricing_inputs: strings("pricing_inputs"),
        corporate_action_factors: text("corporate_action_factors"),
        calculation_version: text("calculation_version"),
        test_cases,
    };
    match refused(&package).first() {
        Some((field, words)) => Err(format!("calculation_package.{field}: {words}")),
        None => Ok(Some(package)),
    }
}

/// What is wrong with a package, each by its field and in words: a model
/// the engine does not ship, a version below one, no pricing input, no
/// calculation version, no test case, or a case whose inputs or answer is
/// not a JSON object. Empty when it stands.
pub fn refused(package: &CalculationPackage) -> Vec<(String, String)> {
    let mut wrong = Vec::new();
    if !MODELS.contains(&package.model_id.as_str()) {
        wrong.push((
            "model_id".to_string(),
            format!(
                "{:?} is not a model the engine ships; v26's are {}",
                package.model_id,
                MODELS.join(", ")
            ),
        ));
    }
    if package.model_version < 1 {
        wrong.push((
            "model_version".to_string(),
            "a model's version is a number from 1".to_string(),
        ));
    }
    if package
        .pricing_inputs
        .iter()
        .all(|kind| kind.trim().is_empty())
    {
        wrong.push((
            "pricing_inputs".to_string(),
            "a package names the kinds of market object it reads".to_string(),
        ));
    }
    if package.calculation_version.trim().is_empty() {
        wrong.push((
            "calculation_version".to_string(),
            "a package states its calculation version, which every valuation names".to_string(),
        ));
    }
    if package.test_cases.is_empty() {
        wrong.push((
            "test_cases".to_string(),
            "a package carries test cases with exact answers".to_string(),
        ));
    }
    for (i, case) in package.test_cases.iter().enumerate() {
        if case.name.trim().is_empty() {
            wrong.push((
                format!("test_cases[{i}].name"),
                "a test case is named".to_string(),
            ));
        }
        for (field, text) in [("inputs", &case.inputs), ("expected", &case.expected)] {
            let object = serde_json::from_str::<serde_json::Value>(text)
                .ok()
                .is_some_and(|value| value.is_object());
            if !object {
                wrong.push((
                    format!("test_cases[{i}].{field}"),
                    "canonical JSON, an object".to_string(),
                ));
            }
        }
    }
    wrong
}

/// A package as a person reads it on a template's line: `quoted_price v1,
/// calculation 1, 3 test cases`.
pub fn words(package: &CalculationPackage) -> String {
    let cases = package.test_cases.len();
    format!(
        "{} v{}, calculation {}, {cases} test case{}",
        package.model_id,
        package.model_version,
        package.calculation_version,
        if cases == 1 { "" } else { "s" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn equity() -> serde_json::Value {
        serde_json::json!({
            "model_id": "quoted_price",
            "model_version": 1,
            "pricing_inputs": ["close"],
            "corporate_action_factors": "the template's split and distribution factors",
            "calculation_version": "1",
            "test_cases": [{
                "name": "10 shares at a close of 227.50 USD, base USD",
                "inputs": "{\"close\":\"227.50\",\"fx\":\"1\",\"quantity\":\"10\"}",
                "expected": "{\"market_value\":\"2275.00\"}"
            }]
        })
    }

    #[test]
    fn a_package_reads_from_the_platforms_json_and_says_itself() {
        let package = package_from_json(Some(&equity())).unwrap().unwrap();
        assert_eq!(package.model_id, "quoted_price");
        assert_eq!(package.model_version, 1);
        assert_eq!(package.pricing_inputs, vec!["close".to_string()]);
        assert_eq!(package.calculation_version, "1");
        assert_eq!(package.test_cases.len(), 1);
        assert_eq!(
            words(&package),
            "quoted_price v1, calculation 1, 1 test case"
        );
        assert!(refused(&package).is_empty());
    }

    #[test]
    fn none_is_no_package_and_a_wrong_one_is_refused_naming_its_field() {
        assert_eq!(package_from_json(None), Ok(None));
        assert_eq!(package_from_json(Some(&serde_json::Value::Null)), Ok(None));
        let mut unknown = equity();
        unknown["model_id"] = "black_scholes".into();
        let said = package_from_json(Some(&unknown)).unwrap_err();
        assert!(said.starts_with("calculation_package.model_id:"), "{said}");
        let mut no_cases = equity();
        no_cases["test_cases"] = serde_json::json!([]);
        let said = package_from_json(Some(&no_cases)).unwrap_err();
        assert!(
            said.starts_with("calculation_package.test_cases:"),
            "{said}"
        );
        let mut bad_answer = equity();
        bad_answer["test_cases"][0]["expected"] = "2275.00".into();
        let said = package_from_json(Some(&bad_answer)).unwrap_err();
        assert!(
            said.starts_with("calculation_package.test_cases[0].expected"),
            "{said}"
        );
    }
}
