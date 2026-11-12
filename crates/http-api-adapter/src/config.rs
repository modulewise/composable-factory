//! The factory's config, and the WIT it generates from that config.
//!
//! The runtime flattens nested config tables into dot-delimited keys, so the
//! config below arrives as `names.fields`, `endpoints.get-forecast.template`,
//! and so on:
//!
//! ```toml
//! [component.url-api-factory.config]
//! package = "example:weather"
//! interface = "weather"
//! types = '''
//!   record forecast { properties: forecast-properties }
//!   ...
//! '''
//!
//! [component.url-api-factory.config.names]
//! fields = "camelCase"
//! enums = "PascalCase"
//!
//! [component.url-api-factory.config.overrides.fields]
//! id = "@id"
//!
//! [component.url-api-factory.config.headers]
//! User-Agent = "my-weather-app/0.1"
//!
//! [component.url-api-factory.config.endpoints.get-forecast]
//! template = "GET https://api.weather.gov/gridpoints/{wfo}/{x:u32},{y:u32}/forecast"
//! result = "forecast"
//! ```

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};

use crate::names::{NameMap, Rules};
use crate::template::Template;

/// The package of the generated interface, unless configured.
const DEFAULT_PACKAGE: &str = "generated:api";

/// The name of the generated interface, unless configured.
const DEFAULT_INTERFACE: &str = "endpoints";

/// The result of an endpoint that declares none: the body, unparsed.
const DEFAULT_RESULT: &str = "string";

/// One function of the generated interface.
#[derive(Debug)]
pub struct Endpoint {
    /// The function's name, from its config key.
    pub name: String,
    pub template: Template,
    /// The WIT type the response body is read into.
    pub result: String,
}

/// Everything the factory is configured with.
#[derive(Debug)]
pub struct Config {
    pub package: String,
    pub interface: String,
    /// WIT type definitions placed in the generated interface, for the
    /// endpoints' results.
    pub types: String,
    pub names: NameMap,
    /// Headers sent with every request, by name.
    pub headers: BTreeMap<String, String>,
    /// The endpoints, by function name.
    pub endpoints: Vec<Endpoint>,
}

impl Config {
    /// The config from its flattened `(key, value)` entries.
    pub fn parse(entries: impl IntoIterator<Item = (String, String)>) -> Result<Self> {
        let mut package = DEFAULT_PACKAGE.to_string();
        let mut interface = DEFAULT_INTERFACE.to_string();
        let mut types = String::new();
        let mut fields = Rules::default();
        let mut enums = Rules::default();
        let mut field_overrides = Vec::new();
        let mut enum_overrides = Vec::new();
        let mut headers = BTreeMap::new();
        // Each endpoint's template and result, by name, as they arrive.
        let mut templates: BTreeMap<String, String> = BTreeMap::new();
        let mut results: BTreeMap<String, String> = BTreeMap::new();

        for (key, value) in entries {
            match key.as_str() {
                "package" => package = value,
                "interface" => interface = value,
                "types" => types = value,
                "names.fields" => fields = Rules::new(value.parse()?),
                "names.enums" => enums = Rules::new(value.parse()?),
                _ => {
                    if let Some(wit) = key.strip_prefix("overrides.fields.") {
                        field_overrides.push((wit.to_string(), value));
                    } else if let Some(wit) = key.strip_prefix("overrides.enums.") {
                        enum_overrides.push((wit.to_string(), value));
                    } else if let Some(name) = key.strip_prefix("headers.") {
                        headers.insert(name.to_string(), value);
                    } else if let Some(endpoint) = key.strip_prefix("endpoints.") {
                        match endpoint.rsplit_once('.') {
                            Some((name, "template")) => {
                                templates.insert(name.to_string(), value);
                            }
                            Some((name, "result")) => {
                                results.insert(name.to_string(), value);
                            }
                            _ => bail!(
                                "unknown config key '{key}' \
                                 (an endpoint has a 'template' and an optional 'result')"
                            ),
                        }
                    } else {
                        bail!("unknown config key '{key}'");
                    }
                }
            }
        }

        // Overrides apply whichever order the entries arrived in.
        for (wit, json) in field_overrides {
            fields = fields.with_override(wit, json);
        }
        for (wit, json) in enum_overrides {
            enums = enums.with_override(wit, json);
        }

        if let Some(name) = results.keys().find(|name| !templates.contains_key(*name)) {
            bail!("endpoint '{name}' has a result but no template");
        }
        if templates.is_empty() {
            bail!("no endpoints configured");
        }
        let endpoints = templates
            .into_iter()
            .map(|(name, template)| {
                let template = Template::parse(&template)
                    .with_context(|| format!("in the template of endpoint '{name}'"))?;
                let result = results
                    .remove(&name)
                    .unwrap_or_else(|| DEFAULT_RESULT.to_string());
                Ok(Endpoint {
                    name,
                    template,
                    result,
                })
            })
            .collect::<Result<_>>()?;

        Ok(Config {
            package,
            interface,
            types,
            names: NameMap { fields, enums },
            headers,
            endpoints,
        })
    }

    /// The WIT package declaring the interface the generated component
    /// exports: the configured types, an `http-error` for a request without a
    /// successful response, and one function per endpoint.
    ///
    /// A function or param name that is a WIT keyword (such as `type`) is
    /// written as `%name`, so it still parses.
    pub fn wit(&self) -> String {
        let mut wit = format!(
            "package {};\n\ninterface {} {{\n{HTTP_ERROR}",
            self.package, self.interface
        );
        if !self.types.trim().is_empty() {
            wit.push_str(&format!("\n{}\n", self.types.trim_end()));
        }
        for endpoint in &self.endpoints {
            let params: Vec<String> = endpoint
                .template
                .variables
                .iter()
                .map(|variable| format!("{}: {}", escape(&variable.name), variable.ty))
                .collect();
            wit.push_str(&format!(
                "\n    {}: async func({}) -> result<{}, http-error>;\n",
                escape(&endpoint.name),
                params.join(", "),
                endpoint.result
            ));
        }
        wit.push_str("}\n");
        wit
    }
}

/// The error of every endpoint. Its cases match templated-http-client's
/// `error`.
const HTTP_ERROR: &str = "    /// A response whose status is not 2xx, with its unparsed body.
    record error-response {
        status: u16,
        body: string,
    }

    variant http-error {
        /// The server responded with a status that is not 2xx.
        response(error-response),
        /// No usable response: a bad template, a connection failure, a
        /// redirect limit, or a body that is not UTF-8.
        failed(string),
    }
";

/// `name` as WIT text: `%name` if it is a keyword, else unchanged.
fn escape(name: &str) -> String {
    if is_keyword(name) {
        format!("%{name}")
    } else {
        name.to_string()
    }
}

/// Whether `name` is a WIT keyword. A copy of wit-component's private
/// `is_keyword` (`src/printing.rs` in wit-component 0.263), which escapes the
/// same names when printing WIT.
fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "use"
            | "type"
            | "func"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "s8"
            | "s16"
            | "s32"
            | "s64"
            | "f32"
            | "f64"
            | "float32"
            | "float64"
            | "char"
            | "resource"
            | "record"
            | "flags"
            | "variant"
            | "enum"
            | "bool"
            | "string"
            | "option"
            | "result"
            | "future"
            | "stream"
            | "list"
            | "own"
            | "borrow"
            | "_"
            | "as"
            | "from"
            | "static"
            | "interface"
            | "tuple"
            | "world"
            | "import"
            | "export"
            | "package"
            | "with"
            | "include"
            | "constructor"
            | "error-context"
            | "async"
            | "map"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use composable_factory::wit::PackageSource;

    fn entries(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// Three weather.gov endpoints, one with a typed result.
    fn weather() -> Vec<(String, String)> {
        entries(&[
            ("package", "example:weather"),
            ("interface", "weather"),
            (
                "types",
                "    record forecast { properties: forecast-properties }
    record forecast-properties { periods: list<period> }
    record period { name: string, temperature: s32, short-forecast: string }",
            ),
            ("names.fields", "camelCase"),
            ("names.enums", "PascalCase"),
            ("overrides.fields.id", "@id"),
            ("headers.User-Agent", "my-weather-app/0.1"),
            (
                "endpoints.get-forecast.template",
                "GET https://api.weather.gov/gridpoints/{wfo}/{x:u32},{y:u32}/forecast",
            ),
            ("endpoints.get-forecast.result", "forecast"),
            (
                "endpoints.get-point.template",
                "GET https://api.weather.gov/points/{latitude:f64},{longitude:f64}",
            ),
            (
                "endpoints.get-alerts.template",
                "GET https://api.weather.gov/alerts/active/area/{area}",
            ),
        ])
    }

    #[test]
    fn the_config_is_read_from_flattened_keys() {
        let config = Config::parse(weather()).unwrap();
        assert_eq!(config.package, "example:weather");
        assert_eq!(config.interface, "weather");
        assert_eq!(
            config.headers.get("User-Agent").map(String::as_str),
            Some("my-weather-app/0.1")
        );
        assert_eq!(config.names.fields.apply("id"), "@id");
        assert_eq!(config.names.fields.apply("short-forecast"), "shortForecast");
        assert_eq!(config.names.enums.apply("severe"), "Severe");
        let names: Vec<&str> = config.endpoints.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["get-alerts", "get-forecast", "get-point"]);
        assert_eq!(config.endpoints[0].result, "string");
        assert_eq!(config.endpoints[1].result, "forecast");
    }

    #[test]
    fn the_generated_wit_declares_one_function_per_endpoint() {
        let config = Config::parse(weather()).unwrap();
        assert_eq!(
            config.wit(),
            "package example:weather;

interface weather {
    /// A response whose status is not 2xx, with its unparsed body.
    record error-response {
        status: u16,
        body: string,
    }

    variant http-error {
        /// The server responded with a status that is not 2xx.
        response(error-response),
        /// No usable response: a bad template, a connection failure, a
        /// redirect limit, or a body that is not UTF-8.
        failed(string),
    }

    record forecast { properties: forecast-properties }
    record forecast-properties { periods: list<period> }
    record period { name: string, temperature: s32, short-forecast: string }

    get-alerts: async func(area: string) -> result<string, http-error>;

    get-forecast: async func(wfo: string, x: u32, y: u32) -> result<forecast, http-error>;

    get-point: async func(latitude: f64, longitude: f64) -> result<string, http-error>;
}
"
        );
        PackageSource::from_text(&config.wit())
            .expect("the generated WIT must parse")
            .interface("weather")
            .expect("the configured interface");
    }

    #[test]
    fn only_keyword_names_are_escaped() {
        let config = Config::parse(entries(&[
            (
                "endpoints.get-by-type.template",
                "GET https://example.com/items/{type}",
            ),
            ("endpoints.list.template", "GET https://example.com/items"),
        ]))
        .unwrap();
        assert_eq!(config.package, DEFAULT_PACKAGE);
        assert_eq!(
            config.names.fields.apply("a-b"),
            "a-b",
            "identity by default"
        );
        let wit = config.wit();
        assert!(
            wit.contains("    get-by-type: async func(%type: string)"),
            "{wit}"
        );
        assert!(wit.contains("    %list: async func()"), "{wit}");
        PackageSource::from_text(&wit).expect("`%type` and `%list` must parse");
    }

    #[test]
    fn an_unknown_key_is_rejected() {
        let error = Config::parse(entries(&[("name.fields", "camelCase")])).unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown config key 'name.fields'"),
            "{error:#}"
        );
        let error = Config::parse(entries(&[("endpoints.get-x.method", "GET")])).unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown config key 'endpoints.get-x.method'"),
            "{error:#}"
        );
    }

    #[test]
    fn a_result_without_a_template_is_rejected() {
        let error = Config::parse(entries(&[
            ("endpoints.get-x.template", "GET https://example.com/x"),
            ("endpoints.get-y.result", "string"),
        ]))
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("endpoint 'get-y' has a result but no template"),
            "{error:#}"
        );
    }

    #[test]
    fn a_config_without_endpoints_is_rejected() {
        let error = Config::parse(entries(&[("interface", "weather")])).unwrap_err();
        assert!(
            format!("{error:#}").contains("no endpoints configured"),
            "{error:#}"
        );
    }

    #[test]
    fn a_template_error_names_its_endpoint() {
        let error = Config::parse(entries(&[(
            "endpoints.create.template",
            "POST https://example.com/items",
        )]))
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("in the template of endpoint 'create'"),
            "{message}"
        );
        assert!(
            message.contains("POST requires a request body"),
            "{message}"
        );
    }

    #[test]
    fn an_unknown_convention_is_rejected() {
        let error = Config::parse(entries(&[
            ("names.fields", "kebab"),
            ("endpoints.get-x.template", "GET https://example.com/x"),
        ]))
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("unknown naming convention 'kebab'"),
            "{error:#}"
        );
        // The parsed convention is what the names follow.
        let config = Config::parse(entries(&[
            ("names.enums", "snake_case"),
            ("endpoints.get-x.template", "GET https://example.com/x"),
        ]))
        .unwrap();
        assert_eq!(config.names.enums.apply("all-clear"), "all_clear");
    }
}
