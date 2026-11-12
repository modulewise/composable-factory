//! RFC 6570 simple string expansion. Each `{name}` is replaced by its value,
//! with percent-encoding for every character outside the unreserved set.

use std::collections::BTreeMap;

/// A variable's value, as text before encoding.
pub enum Value {
    Text(String),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Boolean(bool),
}

impl Value {
    fn render(&self, name: &str) -> Result<String, String> {
        Ok(match self {
            Value::Text(text) => text.clone(),
            Value::Signed(n) => n.to_string(),
            Value::Unsigned(n) => n.to_string(),
            Value::Float(n) if n.is_finite() => n.to_string(),
            Value::Float(n) => {
                return Err(format!("variable '{name}' is {n}, not a finite number"));
            }
            Value::Boolean(b) => b.to_string(),
        })
    }
}

/// `template` with each variable replaced by its encoded value. A variable
/// without a value, or an operator other than simple expansion, is an error.
pub fn expand(template: &str, values: &BTreeMap<String, Value>) -> Result<String, String> {
    let mut expanded = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        expanded.push_str(&rest[..open]);
        let close = rest[open..]
            .find('}')
            .ok_or_else(|| format!("unclosed '{{' in '{template}'"))?;
        let name = &rest[open + 1..open + close];
        if name.starts_with(|c: char| "+#./;?&=,!@|".contains(c)) {
            return Err(format!(
                "the URI template expression '{{{name}}}' is not supported"
            ));
        }
        let value = values
            .get(name)
            .ok_or_else(|| format!("no value for template variable '{name}'"))?;
        encode(&value.render(name)?, &mut expanded);
        rest = &rest[open + close + 1..];
    }
    expanded.push_str(rest);
    Ok(expanded)
}

/// Append `text`, percent-encoding each UTF-8 byte that is not unreserved
/// (`ALPHA / DIGIT / "-" / "." / "_" / "~"`).
fn encode(text: &str, out: &mut String) {
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(entries: Vec<(&str, Value)>) -> BTreeMap<String, Value> {
        entries
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect()
    }

    #[test]
    fn each_variable_is_replaced_by_its_value() {
        let expanded = expand(
            "https://api.weather.gov/gridpoints/{wfo}/{x},{y}/forecast",
            &values(vec![
                ("wfo", Value::Text("TOP".into())),
                ("x", Value::Unsigned(31)),
                ("y", Value::Unsigned(80)),
            ]),
        );
        assert_eq!(
            expanded.unwrap(),
            "https://api.weather.gov/gridpoints/TOP/31,80/forecast"
        );
    }

    #[test]
    fn numbers_and_bools_render_as_text() {
        let expanded = expand(
            "/points/{lat},{lon}?n={n}&b={b}",
            &values(vec![
                ("lat", Value::Float(39.7456)),
                ("lon", Value::Float(-97.0892)),
                ("n", Value::Signed(-3)),
                ("b", Value::Boolean(true)),
            ]),
        );
        assert_eq!(expanded.unwrap(), "/points/39.7456,-97.0892?n=-3&b=true");
    }

    #[test]
    fn reserved_and_non_ascii_characters_are_encoded() {
        let expanded = expand(
            "/items/{id}",
            &values(vec![("id", Value::Text("a/b c?é".into()))]),
        );
        assert_eq!(expanded.unwrap(), "/items/a%2Fb%20c%3F%C3%A9");
    }

    #[test]
    fn a_variable_without_a_value_is_an_error() {
        let error = expand("/items/{id}", &BTreeMap::new()).unwrap_err();
        assert!(
            error.contains("no value for template variable 'id'"),
            "{error}"
        );
    }

    #[test]
    fn a_non_finite_float_is_an_error() {
        let error = expand("/{n}", &values(vec![("n", Value::Float(f64::NAN))])).unwrap_err();
        assert!(error.contains("not a finite number"), "{error}");
    }

    #[test]
    fn an_operator_is_an_error() {
        let error = expand("/alerts{?area}", &BTreeMap::new()).unwrap_err();
        assert!(error.contains("'{?area}' is not supported"), "{error}");
    }

    #[test]
    fn an_unclosed_brace_is_an_error() {
        assert!(expand("/items/{id", &BTreeMap::new()).is_err());
    }
}
