//! A `ComponentBuilder` that builds one function per configured endpoint, each
//! sending its request through templated-http-client and reading the response
//! into the endpoint's result type.

use anyhow::{Context, Result, anyhow, bail};

use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, Kind, Value, ValueSpec, arm};
use composable_factory::{ComponentBuilder, World};

use crate::config::{Config, Endpoint};
use crate::deserializer::JsonDeserializer;

/// templated-http-client's `client` interface, which sends each request.
const CLIENT_WIT: &str = include_str!("../wit/client.wit");

/// json-mapper's interfaces, for reading a JSON body into a typed result.
const JSON_MAPPER_WIT: &str = include_str!("../wit/mapper.wit");

pub struct Builder {
    config: Config,
}

impl Builder {
    pub fn new(config: Config) -> Self {
        Builder { config }
    }

    /// The endpoint an exported function was generated for.
    fn endpoint(&self, function: &ExportedFunction) -> Result<&Endpoint> {
        self.config
            .endpoints
            .iter()
            .find(|endpoint| endpoint.name == function.name())
            .ok_or_else(|| anyhow!("no endpoint for function '{}'", function.name()))
    }

    /// Each template variable's name, paired with its param as
    /// templated-http-client's `template-value`. Narrower numbers widen to the
    /// case's width.
    fn template_values(
        function: &ExportedFunction,
        endpoint: &Endpoint,
    ) -> Result<Vec<(ValueSpec, ValueSpec)>> {
        use wit_parser::Type;

        endpoint
            .template
            .variables
            .iter()
            .map(|variable| {
                let param = function.param(&variable.name)?.receive()?;
                let (case, value) = match variable.ty.as_str() {
                    "string" => ("text", param),
                    "bool" => ("boolean", param),
                    "s64" => ("signed", param),
                    "s8" | "s16" | "s32" => ("signed", param.coerce(Type::S64)?),
                    "u64" => ("unsigned", param),
                    "u8" | "u16" | "u32" => ("unsigned", param.coerce(Type::U64)?),
                    "f64" => ("float", param),
                    "f32" => ("float", param.coerce(Type::F64)?),
                    other => bail!("variable '{}' has type '{other}'", variable.name),
                };
                Ok((
                    ValueSpec::string(&variable.name),
                    ValueSpec::variant(case, value),
                ))
            })
            .collect()
    }

    /// Write a successful `response` into `result` as `ok`: the body itself
    /// for a `string` result, else the body read from JSON.
    fn succeed(&self, response: Value, result: &Value, imports: &Imports) -> Result<()> {
        let body = response.field("body")?;
        let Kind::Result { ok: Some(ok), .. } = result.ty().kind() else {
            bail!("an endpoint returns `result<T, http-error>`");
        };
        if matches!(ok.kind(), Kind::String) {
            return result.write(&ValueSpec::ok(body));
        }
        let mut json = JsonDeserializer::new(
            imports.interface("deserializer")?,
            body,
            self.config.names.clone(),
        )?;
        result
            .write_case("ok")?
            .context("`ok` carries the endpoint's result")?
            .write_with(&mut json)?;
        json.close()
    }

    /// Write templated-http-client's `error` into `result` as `err`, case for
    /// case.
    fn fail(error: Value, result: &Value) -> Result<()> {
        error.dispatch(vec![
            arm("response", |response| {
                let response = response.context("`response` carries the response")?;
                result.write(&ValueSpec::err(ValueSpec::variant(
                    "response",
                    ValueSpec::record([
                        ("status", response.field("status")?),
                        ("body", response.field("body")?),
                    ]),
                )))
            }),
            arm("failed", |message| {
                let message = message.context("`failed` carries a message")?;
                result.write(&ValueSpec::err(ValueSpec::variant("failed", message)))
            }),
        ])
    }
}

impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()> {
        world.add_imports(PackageSource::from_text(CLIENT_WIT)?.interface("client")?)?;
        // Only a typed result is read from JSON.
        if self.config.endpoints.iter().any(|e| e.result != "string") {
            world.add_imports(
                PackageSource::from_text(JSON_MAPPER_WIT)?.interface("deserializer")?,
            )?;
        }
        let generated = PackageSource::from_text(&self.config.wit())
            .context("parsing the WIT generated from the config")?;
        world.add_exports(generated.interface(&self.config.interface)?)
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let endpoint = self.endpoint(function)?;
        let request = imports.interface("client")?.function("request")?;

        let method = request.param("method")?.value()?;
        method.write(&ValueSpec::variant_unit(endpoint.template.method.case()))?;
        let template = request.param("template")?.value()?;
        template.write(&ValueSpec::string(endpoint.template.uri()))?;
        let values = request.param("values")?.value()?;
        values.write(&ValueSpec::map(Self::template_values(function, endpoint)?))?;
        let headers = request.param("headers")?.value()?;
        headers.write(&ValueSpec::map(self.config.headers.iter().map(
            |(name, value)| (ValueSpec::string(name), ValueSpec::string(value)),
        )))?;

        let response = request
            .call(&[method, template, values, headers])?
            .context("request returns a result")?;
        let result = function
            .result()
            .context("an endpoint returns a result")?
            .value();
        response.dispatch(vec![
            arm("ok", |response| {
                let response = response.context("`ok` carries the response")?;
                self.succeed(response, &result, imports)
            }),
            arm("err", |error| {
                Self::fail(error.context("`err` carries the error")?, &result)
            }),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> Config {
        Config::parse(
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string())),
        )
        .expect("a valid config")
    }

    /// Whether a built component imports an interface named `name`.
    fn imports(component: &[u8], name: &str) -> bool {
        let wit_parser::decoding::DecodedWasm::Component(resolve, world) =
            wit_parser::decoding::decode(component).expect("decode")
        else {
            panic!("a component");
        };
        resolve.worlds[world]
            .imports
            .keys()
            .any(|key| resolve.name_world_key(key).contains(name))
    }

    #[test]
    fn string_results_build_without_json_mapping() {
        let bytes = composable_factory::build(&Builder::new(config(&[
            ("headers.User-Agent", "example/0.1"),
            (
                "endpoints.get-alerts.template",
                "GET https://api.weather.gov/alerts/active/area/{area}",
            ),
            (
                "endpoints.get-point.template",
                "GET https://api.weather.gov/points/{latitude:f64},{longitude:f64}",
            ),
        ])))
        .expect("string results must build");
        assert!(imports(&bytes, "composable:templated-http-client/client"));
        assert!(!imports(&bytes, "deserializer"));
    }

    #[test]
    fn a_typed_result_builds_with_every_param_type() {
        let bytes = composable_factory::build(&Builder::new(config(&[
            (
                "types",
                "record forecast { properties: forecast-properties }
                 record forecast-properties { periods: list<period> }
                 record period {
                   number: u32,
                   start-time: string,
                   temperature: s32,
                   short-forecast: string,
                   probability-of-precipitation: option<quantity>,
                 }
                 record quantity { unit-code: string, value: option<f64> }",
            ),
            ("names.fields", "camelCase"),
            ("names.enums", "PascalCase"),
            (
                "endpoints.get-forecast.template",
                "GET https://example.com/{a}/{b:bool}/{c:s8}/{d:s16}/{e:s32}/{f:s64}\
                 /{g:u8}/{h:u16}/{i:u32}/{j:u64}/{k:f32}/{l:f64}",
            ),
            ("endpoints.get-forecast.result", "forecast"),
        ])))
        .expect("a typed result must build");
        assert!(imports(&bytes, "deserializer"));
    }
}
