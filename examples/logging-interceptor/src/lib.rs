//! An interceptor factory that mirrors a target component's exports, forwards
//! to the target's functions as imports, and logs each call and return.

use anyhow::Result;

use composable_factory::wit::{PackageSource, WorldSource};
use composable_factory::world::{ExportedFunction, ImportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World};

const LOGGING_WIT: &str = include_str!("../assets/logging.wit");

const LOGGING_INTERFACE: &str = "logging";

const LEVEL: &str = "info";

pub struct Builder {
    target: Vec<u8>,
}

impl Builder {
    pub fn new(target: Vec<u8>) -> Self {
        Builder { target }
    }
}

impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let target = WorldSource::from_component(&self.target)?;
        if target.exports().is_empty() {
            anyhow::bail!("target has no exports to intercept");
        }

        // Add the target's imports, and mirror them as exports.
        world.add_imports(target.exports())?;
        world.add_exports(target.exports())?;

        let logging = PackageSource::from_text(LOGGING_WIT)?;
        world.add_imports(logging.interface(LOGGING_INTERFACE)?)
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let name = function.name().to_string();

        log(imports, &name, "called")?;
        let args = function
            .params()
            .iter()
            .map(|p| p.receive())
            .collect::<Result<Vec<_>>>()?;
        let result = target_function(
            imports,
            function.qualified_interface_name().as_deref(),
            function.name(),
        )?
        .call(&args)?;
        log(imports, &name, "returned")?;

        // Write the target's result.
        match (function.result(), result) {
            (Some(dest), Some(value)) => dest.value().write(&ValueSpec::from(value)),
            _ => Ok(()),
        }
    }
}

fn target_function(imports: &Imports, iface: Option<&str>, func: &str) -> Result<ImportedFunction> {
    match iface {
        Some(iface) => imports.interface(iface)?.function(func),
        None => imports.function(func),
    }
}

fn log(imports: &Imports, context: &str, message: &str) -> Result<()> {
    let log_fn = imports.interface(LOGGING_INTERFACE)?.function("log")?;

    let level = log_fn.param("level")?.value()?;
    level.write(&ValueSpec::variant_unit(LEVEL))?;

    let context_arg = log_fn.param("context")?.value()?;
    context_arg.write(&ValueSpec::string(context))?;

    let message_arg = log_fn.param("message")?.value()?;
    message_arg.write(&ValueSpec::string(message))?;

    log_fn.call(&[level, context_arg, message_arg])?;

    Ok(())
}

wit_bindgen::generate!({
    path: "wit",
    world: "logging-interceptor-factory",
    generate_all,
});

struct Factory;

impl exports::composable::factory::factory::Guest for Factory {
    async fn build() -> Result<Vec<u8>, String> {
        let source = wasi::config::store::get("target")
            .map_err(|e| format!("reading config 'target': {e:?}"))?
            .ok_or_else(|| "no target in config".to_string())?;

        let target = composable::factory::loader::load(source).await?;

        composable_factory::build(&Builder::new(target)).map_err(|e| format!("{e:#}"))
    }
}

export!(Factory);
