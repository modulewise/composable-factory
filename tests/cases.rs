//! A factory writing the case of a variant-like value that an import selects
//! at runtime.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, Value, WriteVisitor};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store, Trap};

const CASES_WIT: &str = r"package test:cases;
    interface source {
      present: func() -> bool;
      index: func() -> u32;
    }
    interface picker {
      enum color { red, green, blue }
      pick: func() -> color;
    }
    world cases {
      import source;
      export picker;
    }";

/// `pick` returns the case whose index the `source` import reports.
struct Cases;

impl ComponentBuilder for Cases {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let cases = PackageSource::from_text(CASES_WIT)?.world("cases")?;
        world.add_imports(cases.imports())?;
        world.add_exports(cases.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        function
            .result()
            .context("pick returns a color")?
            .value()
            .write_with(&mut Source {
                imports: imports.clone(),
            })
    }
}

/// Supplies the case index the `source` import reports.
struct Source {
    imports: Imports,
}

impl Source {
    fn call(&self, name: &str) -> Result<Value> {
        self.imports
            .interface("source")?
            .function(name)?
            .call(&[])?
            .with_context(|| format!("{name} returns a value"))
    }
}

impl WriteVisitor for Source {
    fn begin_walk(&mut self) -> Result<Value> {
        self.call("present")
    }

    fn case_index(&mut self, _names: &[&str]) -> Result<Value> {
        self.call("index")
    }
}

/// Call `pick` with `index` reported as the case.
fn pick(index: u32) -> Result<Val> {
    let engine = Engine::default();
    let component = Component::new(&engine, build(&Cases)?)?;
    let mut linker = Linker::<()>::new(&engine);
    let mut source = linker.instance("test:cases/source")?;
    source.func_new("present", |_, _, _, results| {
        results[0] = Val::Bool(true);
        Ok(())
    })?;
    let index = Arc::new(AtomicU32::new(index));
    source.func_new("index", move |_, _, _, results| {
        results[0] = Val::U32(index.load(Ordering::SeqCst));
        Ok(())
    })?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let picker = instance
        .get_export_index(&mut store, None, "test:cases/picker")
        .context("the picker export")?;
    let pick = instance
        .get_export_index(&mut store, Some(&picker), "pick")
        .and_then(|index| instance.get_func(&mut store, index))
        .context("the pick function")?;
    let mut results = [Val::Bool(false)];
    pick.call(&mut store, &[], &mut results)?;
    let [result] = results;
    Ok(result)
}

#[test]
fn each_case_index_writes_its_case() -> Result<()> {
    for (index, name) in ["red", "green", "blue"].into_iter().enumerate() {
        assert_eq!(pick(index as u32)?, Val::Enum(name.to_string()));
    }
    Ok(())
}

#[test]
fn an_index_past_the_last_case_traps() {
    let error = pick(3).expect_err("no case matches");
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::UnreachableCodeReached),
        "{error:?}"
    );
}
