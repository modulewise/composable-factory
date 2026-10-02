//! A factory returning each case of a result, with and without a payload.

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store};

const RESULTS_WIT: &str = r"package test:results;
    world results {
      export found: func() -> result<u32, string>;
      export refused: func() -> result<u32, string>;
      export done: func() -> result<_, string>;
      export missing: func() -> result<u32>;
      export failed: func() -> result;
    }";

/// `found` and `refused` return `ok` and `err` with a payload. `done`,
/// `missing` and `failed` return a case that has none.
struct Results;

impl ComponentBuilder for Results {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let results = PackageSource::from_text(RESULTS_WIT)?.world("results")?;
        world.add_exports(results.exports())
    }

    fn build_function(&self, function: &ExportedFunction, _imports: &Imports) -> Result<()> {
        let case = match function.name() {
            "found" => ValueSpec::ok(ValueSpec::u32(7)),
            "refused" => ValueSpec::err(ValueSpec::string("no")),
            "done" => ValueSpec::variant_unit("ok"),
            _ => ValueSpec::variant_unit("err"),
        };
        function
            .result()
            .context("every function returns a result")?
            .value()
            .write(&case)
    }
}

#[test]
fn each_case_is_written_with_its_payload_or_without_one() -> Result<()> {
    let engine = Engine::default();
    let component = Component::new(&engine, build(&Results)?)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::<()>::new(&engine).instantiate(&mut store, &component)?;
    for (name, expected) in [
        ("found", Ok(Some(Box::new(Val::U32(7))))),
        (
            "refused",
            Err(Some(Box::new(Val::String("no".to_string())))),
        ),
        ("done", Ok(None)),
        ("missing", Err(None)),
        ("failed", Err(None)),
    ] {
        let function = instance
            .get_func(&mut store, name)
            .with_context(|| format!("the {name} function"))?;
        let mut results = [Val::Bool(false)];
        function.call(&mut store, &[], &mut results)?;
        assert_eq!(results[0], Val::Result(expected), "{name}");
    }
    Ok(())
}
