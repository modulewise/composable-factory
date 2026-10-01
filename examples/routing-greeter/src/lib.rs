//! A routing greeter factory: the generated greeter routes each call by
//! locale to the greeter configured for it, or to `english` otherwise.

use std::collections::BTreeSet;

use anyhow::{Context, Result};

use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec, exact, prefix};
use composable_factory::{ComponentBuilder, World};

/// The `greeter` interface each route's greeter implements.
const GREETER_WIT: &str = include_str!("../wit/package.wit");

/// The world of the generated routing greeter.
const ROUTING_GREETER_WIT: &str = r"package example:routing-greeter;

world routing-greeter {
    export greet: func(name: string, locale: string) -> string;
}";

/// The greeter for a locale no route matches.
const FALLBACK: &str = "english";

/// A locale, matched exactly or as a prefix, and the greeter it routes to.
struct Route {
    locale: String,
    is_prefix: bool,
    greeter: String,
}

pub struct Builder {
    /// Exact routes first, then prefixes from longest to shortest, so the
    /// most specific route matches whatever order the config lists them in.
    routes: Vec<Route>,
}

impl Builder {
    /// Routes from `<locale> = <greeter>` config entries, where a locale
    /// ending in `*` is a prefix.
    fn new(config: Vec<(String, String)>) -> Self {
        let mut routes: Vec<Route> = config
            .into_iter()
            .map(|(locale, greeter)| match locale.strip_suffix('*') {
                Some(prefix) => Route {
                    locale: prefix.to_string(),
                    is_prefix: true,
                    greeter,
                },
                None => Route {
                    locale,
                    is_prefix: false,
                    greeter,
                },
            })
            .collect();
        routes.sort_by_key(|route| {
            (
                route.is_prefix,
                std::cmp::Reverse(route.locale.len()),
                route.locale.clone(),
            )
        });
        Builder { routes }
    }

    /// Every greeter a route names, and the fallback.
    fn greeters(&self) -> BTreeSet<&str> {
        self.routes
            .iter()
            .map(|route| route.greeter.as_str())
            .chain([FALLBACK])
            .collect()
    }
}

impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let greeter = PackageSource::from_text(GREETER_WIT)?;
        for name in self.greeters() {
            world.add_imports(greeter.interface("greeter")?.named(name)?)?;
        }
        let routing_greeter = PackageSource::from_text(ROUTING_GREETER_WIT)?;
        world.add_exports(routing_greeter.world("routing-greeter")?.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let name = function.param("name")?.receive()?;
        let locale = function.param("locale")?.receive()?;
        let result = function.result().context("greet returns a string")?.value();

        // Call the named greeter and return its greeting.
        let greet = |greeter: &str| -> Result<()> {
            let greeting = imports
                .interface(greeter)?
                .function("greet")?
                .call(std::slice::from_ref(&name))?
                .context("greet returns a string")?;
            result.write(&ValueSpec::from(greeting))
        };
        let greet = &greet;

        let arms = self
            .routes
            .iter()
            .map(|route| match route.is_prefix {
                true => prefix(&route.locale, move |_| greet(&route.greeter)),
                false => exact(&route.locale, move || greet(&route.greeter)),
            })
            .collect();
        locale.match_string(arms, || greet(FALLBACK))
    }
}

wit_bindgen::generate!({
    path: "wit",
    world: "routing-greeter-factory",
    generate_all,
});

struct Factory;

impl exports::composable::factory::factory::Guest for Factory {
    async fn build() -> Result<Vec<u8>, String> {
        let config =
            wasi::config::store::get_all().map_err(|e| format!("reading config: {e:?}"))?;
        composable_factory::build(&Builder::new(config)).map_err(|e| format!("{e:#}"))
    }
}

export!(Factory);
