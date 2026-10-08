//! `MAX_FLAT_PARAMS` (16) as a count of separate params rather than the
//! flats of one: an export receiving 16 and 17 `u8` params, sync and async,
//! and an import called with 16 and 17. Past the limit, the params are one
//! pointer to a record that includes them all as fields.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, Engine, Store};

use crate::support::block_on;

fn params(count: usize) -> String {
    (0..count)
        .map(|i| format!("p{i}: u8"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn wit() -> String {
    let (sixteen, seventeen) = (params(16), params(17));
    format!(
        "package test:params;
        interface sink {{
          put-16: func({sixteen});
          put-17: func({seventeen});
        }}
        world params {{
          import sink;
          export sync-16: func({sixteen}) -> list<u8>;
          export sync-17: func({seventeen}) -> list<u8>;
          export async-16: async func({sixteen}) -> list<u8>;
          export async-17: async func({seventeen}) -> list<u8>;
          export forward-16: func({sixteen});
          export forward-17: func({seventeen});
        }}"
    )
}

/// Each export returns its params as a list, or passes them to the import
/// with the same count.
struct Params;

impl ComponentBuilder for Params {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let params = PackageSource::from_text(&wit())?.world("params")?;
        world.add_imports(params.imports())?;
        world.add_exports(params.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let received = function
            .params()
            .iter()
            .map(|param| param.receive())
            .collect::<Result<Vec<_>>>()?;
        if let Some(count) = function.name().strip_prefix("forward-") {
            imports
                .interface("sink")?
                .function(&format!("put-{count}"))?
                .call(&received)?;
            return Ok(());
        }
        function
            .result()
            .context("returns a list")?
            .value()
            .write(&ValueSpec::list(received.iter().map(ValueSpec::from)))
    }
}

/// Distinct values, so a param read from the wrong place shows.
fn args(count: usize) -> Vec<Val> {
    (0..count).map(|i| Val::U8(0xA0 + i as u8)).collect()
}

/// Call `name` with `count` params, and what it returned or passed.
fn call(name: &str, count: usize) -> Result<Val> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, build(&Params)?)?;
    let received = Arc::new(Mutex::new(None));
    let mut linker = Linker::<()>::new(&engine);
    let mut sink = linker.instance("test:params/sink")?;
    for put in ["put-16", "put-17"] {
        let received = received.clone();
        sink.func_new(put, move |_, _, args, _| {
            *received.lock().unwrap() = Some(Val::List(args.to_vec()));
            Ok(())
        })?;
    }
    let returned = block_on(async {
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let function = instance
            .get_func(&mut store, name)
            .context(name.to_string())?;
        let mut results = vec![Val::Bool(false); function.ty(&store).results().len()];
        let args = args(count);
        store
            .run_concurrent(async |accessor| {
                function
                    .call_concurrent(accessor, &args, &mut results)
                    .await
            })
            .await??;
        anyhow::Ok(results.into_iter().next())
    })?;
    match returned {
        Some(value) => Ok(value),
        None => received
            .lock()
            .unwrap()
            .take()
            .context("the import received nothing"),
    }
}

#[test]
fn sixteen_and_seventeen_params_arrive_in_order() -> Result<()> {
    for count in [16, 17] {
        for kind in ["sync", "async", "forward"] {
            let name = format!("{kind}-{count}");
            assert_eq!(call(&name, count)?, Val::List(args(count)), "{name}");
        }
    }
    Ok(())
}
