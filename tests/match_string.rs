//! A factory routing a string by exact and prefix matches.

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec, exact, prefix};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store};

const ROUTE_WIT: &str = r"package test:route;
    world router { export route: func(name: string) -> string; }";

/// Routes `forecast` exactly, anything starting with `weather-` to the rest of
/// the name, and anything else to `none`.
struct Route;

impl ComponentBuilder for Route {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let router = PackageSource::from_text(ROUTE_WIT)?.world("router")?;
        world.add_exports(router.exports())
    }

    fn build_function(&self, function: &ExportedFunction, _: &Imports) -> Result<()> {
        let name = function.params()[0].receive()?;
        let result = function.result().context("route returns a string")?.value();
        name.match_string(
            vec![
                exact("forecast", || result.write(&ValueSpec::string("exact"))),
                prefix("weather-", |rest| result.write(&ValueSpec::from(rest))),
            ],
            || result.write(&ValueSpec::string("none")),
        )
    }
}

#[test]
fn a_string_is_routed_by_exact_and_prefix_matches() -> Result<()> {
    let bytes = build(&Route)?;

    let engine = Engine::default();
    let component = Component::new(&engine, &bytes)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::<()>::new(&engine).instantiate(&mut store, &component)?;
    let route = instance
        .get_func(&mut store, "route")
        .context("the route function")?;

    for (name, expected) in [
        ("forecast", "exact"),
        // The prefix arm receives the rest of the name.
        ("weather-alerts", "alerts"),
        ("weather-", ""),
        // As long as the exact literal, but differing in its last byte.
        ("forecasx", "none"),
        // Shorter than both literals.
        ("fore", "none"),
        // Starts with the exact literal, but is longer.
        ("forecasts", "none"),
        ("other", "none"),
        ("", "none"),
    ] {
        let mut results = [Val::Bool(false)];
        route.call(&mut store, &[Val::String(name.to_string())], &mut results)?;
        assert_eq!(
            results[0],
            Val::String(expected.to_string()),
            "route({name:?})"
        );
    }
    Ok(())
}
