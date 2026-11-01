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

use crate::v1::{
    AssetClass, CalculationPackage, CalculationTestCase, ExerciseStyle, FieldRequirement,
    ModelBinding,
};

/// The models the packages bind (W12.3): v26's built-in three, from
/// contract v28 the listed options' (ruling 565): the binomial tree with
/// discrete cash dividends, Black-Scholes and Black-76, and from contract
/// v29 the debt templates' `bond_cash_flows` (ruling 984: the platform's
/// packages name it). A package naming another is refused rather than kept,
/// since no implementation exists to equal its test cases.
pub const MODELS: [&str; 7] = [
    "cash",
    "quoted_price",
    "binary_event",
    "american_binomial",
    "black_scholes",
    "black_76",
    "bond_cash_flows",
];

/// The model a package binds for an instrument's terms (contract v28,
/// ruling 576): the first binding whose style and underlying class match,
/// each unspecified matching any; with no bindings, the package's own. `None`
/// where bindings exist and none matches -- refused at publish.
pub fn model_for(
    package: &CalculationPackage,
    style: ExerciseStyle,
    underlying: AssetClass,
) -> Option<(String, i32)> {
    if package.model_bindings.is_empty() {
        return Some((package.model_id.clone(), package.model_version));
    }
    package
        .model_bindings
        .iter()
        .find(|binding| {
            (binding.exercise_style == ExerciseStyle::Unspecified as i32
                || binding.exercise_style == style as i32)
                && (binding.underlying_asset_class == AssetClass::Unspecified as i32
                    || binding.underlying_asset_class == underlying as i32)
        })
        .map(|binding| (binding.model_id.clone(), binding.model_version))
}

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
    let model_bindings = match object.get("model_bindings") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(rows)) => rows
            .iter()
            .map(binding_from_json)
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("calculation_package.model_bindings is a list".into()),
    };
    let package = CalculationPackage {
        model_bindings,
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

/// A template field's requirement level (contract v28, ruling 574): `mint`,
/// `complete` or `optional`, or the enum's name; empty on a version before
/// v28, read from `required`, true as mint and false as optional.
pub fn requirement_of(word: &str, required: bool) -> FieldRequirement {
    let upper = word.trim().to_ascii_uppercase();
    FieldRequirement::from_str_name(&upper)
        .or_else(|| FieldRequirement::from_str_name(&format!("FIELD_REQUIREMENT_{upper}")))
        .filter(|level| *level != FieldRequirement::Unspecified)
        .unwrap_or(if required {
            FieldRequirement::Mint
        } else {
            FieldRequirement::Optional
        })
}

/// One `model_bindings` row from the platform's JSON: its style and the
/// underlying's class by their enum names or in lower case without the
/// prefix (`american`, `equity`), either absent for any.
fn binding_from_json(value: &serde_json::Value) -> Result<ModelBinding, String> {
    let object = value
        .as_object()
        .ok_or("calculation_package.model_bindings[] is an object")?;
    let text = |name: &str| {
        object
            .get(name)
            .and_then(|text| text.as_str())
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let style = match text("exercise_style").to_ascii_uppercase().as_str() {
        "" => ExerciseStyle::Unspecified,
        name => ExerciseStyle::from_str_name(name)
            .or_else(|| ExerciseStyle::from_str_name(&format!("EXERCISE_STYLE_{name}")))
            .ok_or_else(|| {
                format!(
                    "calculation_package.model_bindings[].exercise_style: {name:?} is not a style"
                )
            })?,
    };
    let class = match text("underlying_asset_class").to_ascii_uppercase().as_str() {
        "" => AssetClass::Unspecified,
        name => AssetClass::from_str_name(name)
            .or_else(|| AssetClass::from_str_name(&format!("ASSET_CLASS_{name}")))
            .ok_or_else(|| {
                format!(
                    "calculation_package.model_bindings[].underlying_asset_class: {name:?} is not a class"
                )
            })?,
    };
    let version = match object.get("model_version") {
        Some(serde_json::Value::Number(number)) => number.as_i64().unwrap_or(-1),
        Some(serde_json::Value::String(text)) => text.trim().parse().unwrap_or(-1),
        _ => 0,
    };
    Ok(ModelBinding {
        exercise_style: style as i32,
        underlying_asset_class: class as i32,
        model_id: text("model_id"),
        model_version: i32::try_from(version).unwrap_or(-1),
    })
}

/// What is wrong with a package, each by its field and in words: a model
/// the engine does not ship, a version below one, no pricing input, no
/// calculation version, no test case, or a case whose inputs or answer is
/// not a JSON object. Empty when it stands.
pub fn refused(package: &CalculationPackage) -> Vec<(String, String)> {
    let mut wrong = Vec::new();
    for (i, binding) in package.model_bindings.iter().enumerate() {
        if !MODELS.contains(&binding.model_id.as_str()) {
            wrong.push((
                format!("model_bindings[{i}].model_id"),
                format!(
                    "{:?} is not a model the engine ships; they are {}",
                    binding.model_id,
                    MODELS.join(", ")
                ),
            ));
        }
        if binding.model_version < 1 {
            wrong.push((
                format!("model_bindings[{i}].model_version"),
                "a model's version is a number from 1".to_string(),
            ));
        }
    }
    let bound = !package.model_bindings.is_empty();
    if !bound && !MODELS.contains(&package.model_id.as_str()) {
        wrong.push((
            "model_id".to_string(),
            format!(
                "{:?} is not a model the engine ships; v26's are {}",
                package.model_id,
                MODELS.join(", ")
            ),
        ));
    }
    if !bound && package.model_version < 1 {
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
    let models = if package.model_bindings.is_empty() {
        format!("{} v{}", package.model_id, package.model_version)
    } else {
        package
            .model_bindings
            .iter()
            .map(|binding| {
                let terms: Vec<String> = [
                    ExerciseStyle::try_from(binding.exercise_style)
                        .ok()
                        .filter(|style| *style != ExerciseStyle::Unspecified)
                        .map(|style| short(style.as_str_name(), "EXERCISE_STYLE_")),
                    AssetClass::try_from(binding.underlying_asset_class)
                        .ok()
                        .filter(|class| *class != AssetClass::Unspecified)
                        .map(|class| short(class.as_str_name(), "ASSET_CLASS_")),
                ]
                .into_iter()
                .flatten()
                .collect();
                let terms = if terms.is_empty() {
                    "any".to_string()
                } else {
                    terms.join(", ")
                };
                format!("{} v{} ({terms})", binding.model_id, binding.model_version)
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    format!(
        "{models}, calculation {}, {cases} test case{}",
        package.calculation_version,
        if cases == 1 { "" } else { "s" }
    )
}

fn short(name: &str, prefix: &str) -> String {
    name.strip_prefix(prefix)
        .unwrap_or(name)
        .to_ascii_lowercase()
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
        unknown["model_id"] = "heston".into();
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

    #[test]
    fn a_listed_options_package_binds_its_model_by_style_and_underlying() {
        let mut value = equity();
        value["model_id"] = serde_json::json!("");
        value["model_version"] = serde_json::json!(0);
        value["model_bindings"] = serde_json::json!([
            {"exercise_style": "american", "underlying_asset_class": "equity",
             "model_id": "american_binomial", "model_version": 1},
            {"exercise_style": "EXERCISE_STYLE_EUROPEAN",
             "model_id": "black_scholes", "model_version": 1}
        ]);
        let package = package_from_json(Some(&value)).unwrap().unwrap();
        assert_eq!(
            model_for(&package, ExerciseStyle::American, AssetClass::Equity),
            Some(("american_binomial".to_string(), 1))
        );
        assert_eq!(
            model_for(&package, ExerciseStyle::European, AssetClass::Referential),
            Some(("black_scholes".to_string(), 1))
        );
        assert_eq!(
            model_for(&package, ExerciseStyle::American, AssetClass::Referential),
            None
        );
        assert!(words(&package)
            .starts_with("american_binomial v1 (american, equity); black_scholes v1 (european)"));
        value["model_bindings"][1]["model_id"] = serde_json::json!("heston");
        assert!(package_from_json(Some(&value))
            .unwrap_err()
            .starts_with("calculation_package.model_bindings[1].model_id"));
    }
}
