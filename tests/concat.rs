//! A factory joining lists from several imports.

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, Engine, Store};

const ITEM_WIT: &str = r"package test:catalog;
    interface types { record entry { name: string } }
    interface item {
      use types.{entry};
      describe: func() -> entry;
    }";

const CATALOG_WIT: &str = r"package test:catalog;
    interface types { record entry { name: string } }
    interface catalog {
      use types.{entry};
      entries: func() -> list<entry>;
    }";

/// A catalog of two items and a nested catalog. The items' `entry` and the
/// catalog's come from separate sources, each through its own `use` alias.
struct Catalog;

impl ComponentBuilder for Catalog {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let item = PackageSource::from_text(ITEM_WIT)?;
        let catalog = PackageSource::from_text(CATALOG_WIT)?;
        world.add_imports(item.interface("item")?.named("first")?)?;
        world.add_imports(item.interface("item")?.named("second")?)?;
        world.add_imports(catalog.interface("catalog")?.named("nested")?)?;
        world.add_exports(catalog.interface("catalog")?)
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let describe = |name: &str| -> Result<_> {
            imports
                .interface(name)?
                .function("describe")?
                .call(&[])?
                .context("describe returns an entry")
        };
        let (first, second) = (describe("first")?, describe("second")?);
        let nested = imports
            .interface("nested")?
            .function("entries")?
            .call(&[])?
            .context("entries returns a list")?;
        function
            .result()
            .context("entries returns a list")?
            .value()
            .write(&ValueSpec::concat([
                ValueSpec::list([first, second]),
                ValueSpec::from(nested),
            ]))
    }
}

fn entry(name: &str) -> Val {
    Val::Record(vec![("name".to_string(), Val::String(name.to_string()))])
}

#[test]
fn entries_from_items_and_a_nested_catalog_are_joined() -> Result<()> {
    let bytes = build(&Catalog)?;

    let mut config = Config::new();
    config.wasm_component_model_implements(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, &bytes)?;

    let mut linker = Linker::<()>::new(&engine);
    linker.instance("test:catalog/types")?;
    for (import, name) in [("first", "a"), ("second", "b")] {
        linker
            .instance(import)?
            .func_new("describe", move |_, _, _, results| {
                results[0] = entry(name);
                Ok(())
            })?;
    }
    linker
        .instance("nested")?
        .func_new("entries", |_, _, _, results| {
            results[0] = Val::List(vec![entry("c"), entry("d"), entry("e")]);
            Ok(())
        })?;

    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let catalog = instance
        .get_export_index(&mut store, None, "test:catalog/catalog")
        .context("the catalog export")?;
    let entries = instance
        .get_export_index(&mut store, Some(&catalog), "entries")
        .and_then(|index| instance.get_func(&mut store, index))
        .context("the entries function")?;

    let mut results = [Val::Bool(false)];
    entries.call(&mut store, &[], &mut results)?;
    let expected = ["a", "b", "c", "d", "e"].map(entry).to_vec();
    assert_eq!(results[0], Val::List(expected));
    Ok(())
}
