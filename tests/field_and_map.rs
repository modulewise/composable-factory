//! A factory reading record fields, and mapping a list into a new list.

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store};

const CATALOG_WIT: &str = r"package test:catalog;
    interface entries {
      record entry { name: string, size: u32, tags: list<string> }
      name-of: func(e: entry) -> string;
      rename: func(entries: list<entry>) -> list<entry>;
    }
    world catalog { export entries; }";

/// `name-of` returns an entry's name. `rename` returns the entries with `x-`
/// before each name, and every other field copied.
struct Catalog;

impl ComponentBuilder for Catalog {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let catalog = PackageSource::from_text(CATALOG_WIT)?.world("catalog")?;
        world.add_exports(catalog.exports())
    }

    fn build_function(&self, function: &ExportedFunction, _: &Imports) -> Result<()> {
        let param = function.params()[0].receive()?;
        let result = function.result().context("every function returns")?.value();
        match function.name() {
            "name-of" => result.write(&ValueSpec::from(param.field("name")?)),
            "rename" => {
                let renamed = param.map(result.ty(), |entry| {
                    Ok(ValueSpec::record([
                        (
                            "name",
                            ValueSpec::concat([
                                ValueSpec::string("x-"),
                                entry.field("name")?.into(),
                            ]),
                        ),
                        ("size", entry.field("size")?.into()),
                        ("tags", entry.field("tags")?.into()),
                    ]))
                })?;
                result.write(&ValueSpec::from(renamed))
            }
            other => anyhow::bail!("unexpected function '{other}'"),
        }
    }
}

fn entry(name: &str, size: u32, tags: &[&str]) -> Val {
    Val::Record(vec![
        ("name".to_string(), Val::String(name.to_string())),
        ("size".to_string(), Val::U32(size)),
        (
            "tags".to_string(),
            Val::List(
                tags.iter()
                    .map(|tag| Val::String(tag.to_string()))
                    .collect(),
            ),
        ),
    ])
}

#[test]
fn fields_are_read_and_a_list_is_mapped() -> Result<()> {
    let bytes = build(&Catalog)?;

    let engine = Engine::default();
    let component = Component::new(&engine, &bytes)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::<()>::new(&engine).instantiate(&mut store, &component)?;
    let entries = instance
        .get_export_index(&mut store, None, "test:catalog/entries")
        .context("the entries export")?;
    let mut function = |name: &str| {
        instance
            .get_export_index(&mut store, Some(&entries), name)
            .and_then(|index| instance.get_func(&mut store, index))
            .with_context(|| format!("the {name} function"))
    };
    let (name_of, rename) = (function("name-of")?, function("rename")?);

    let mut results = [Val::Bool(false)];
    name_of.call(&mut store, &[entry("a", 1, &["red"])], &mut results)?;
    assert_eq!(results[0], Val::String("a".to_string()));

    let given = Val::List(vec![entry("a", 1, &["red", "large"]), entry("b", 2, &[])]);
    rename.call(&mut store, &[given], &mut results)?;
    let expected = Val::List(vec![
        entry("x-a", 1, &["red", "large"]),
        entry("x-b", 2, &[]),
    ]);
    assert_eq!(results[0], expected);

    rename.call(&mut store, &[Val::List(Vec::new())], &mut results)?;
    assert_eq!(
        results[0],
        Val::List(Vec::new()),
        "an empty list maps to an empty list"
    );
    Ok(())
}
