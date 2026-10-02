//! A factory whose bodies allocate past their initial memory, and whose
//! allocations sized at runtime trap rather than wrap.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, Value, ValueSpec, WriteVisitor};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Func, Instance, Linker, Val};
use wasmtime::{Engine, Store, Trap};

const SIZED_WIT: &str = r"package test:sized;
    interface source {
      present: func() -> bool;
      length: func() -> u32;
    }
    world sized {
      import source;
      export double: func(data: list<u8>) -> list<u8>;
      export bytes: func() -> list<u8>;
      export words: func() -> list<u64>;
      export text: func() -> string;
    }";

/// The literal `text` returns, interned into a data segment larger than a page.
fn text() -> String {
    (0..100_000)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect()
}

/// `double` joins its argument with itself. `text` returns a literal. `bytes`
/// and `words` are built by a visitor, with length from the `source` import.
struct Sized;

impl ComponentBuilder for Sized {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let sized = PackageSource::from_text(SIZED_WIT)?.world("sized")?;
        world.add_imports(sized.imports())?;
        world.add_exports(sized.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let result = function.result().context("every function returns")?.value();
        match function.name() {
            "double" => {
                let data = function.param("data")?.receive()?;
                result.write(&ValueSpec::concat([
                    ValueSpec::from(&data),
                    ValueSpec::from(&data),
                ]))
            }
            "text" => result.write(&ValueSpec::string(text())),
            _ => result.write_with(&mut Source {
                imports: imports.clone(),
            }),
        }
    }
}

/// Supplies a list (of zeros) whose length the `source` import reports.
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

    fn length(&mut self) -> Result<Value> {
        self.call("length")
    }

    fn on_u8(&mut self) -> Result<ValueSpec> {
        Ok(ValueSpec::u8(0))
    }

    fn on_u64(&mut self) -> Result<ValueSpec> {
        Ok(ValueSpec::u64(0))
    }
}

/// An instance of the component, with `length` reporting what `length` holds.
fn instantiate(length: Arc<AtomicU32>) -> Result<(Store<()>, Instance)> {
    let engine = Engine::default();
    let component = Component::new(&engine, build(&Sized)?)?;
    let mut linker = Linker::<()>::new(&engine);
    let mut source = linker.instance("test:sized/source")?;
    source.func_new("present", |_, _, _, results| {
        results[0] = Val::Bool(true);
        Ok(())
    })?;
    source.func_new("length", move |_, _, _, results| {
        results[0] = Val::U32(length.load(Ordering::SeqCst));
        Ok(())
    })?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    Ok((store, instance))
}

fn function(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<Func> {
    instance
        .get_func(&mut *store, name)
        .with_context(|| format!("the {name} function"))
}

/// Call `name`, which takes no arguments, expecting it to trap with the
/// `unreachable` a failed check emits, rather than any other failure.
fn assert_traps(length: u32, name: &str) -> Result<()> {
    let (mut store, instance) = instantiate(Arc::new(AtomicU32::new(length)))?;
    let function = function(&mut store, &instance, name)?;
    let mut results = [Val::Bool(false)];
    let error = function
        .call(&mut store, &[], &mut results)
        .expect_err("the allocation must trap");
    assert_eq!(
        error.downcast_ref::<Trap>(),
        Some(&Trap::UnreachableCodeReached),
        "{error:?}"
    );
    Ok(())
}

#[test]
fn memory_grows_past_its_initial_size() -> Result<()> {
    let (mut store, instance) = instantiate(Arc::default())?;
    let double = function(&mut store, &instance, "double")?;
    // The argument is lowered into the component's memory, and the result
    // allocated there, both past the initial memory.
    let data: Vec<Val> = (0..100_000).map(|i| Val::U8((i % 251) as u8)).collect();
    let mut results = [Val::Bool(false)];
    double.call(&mut store, &[Val::List(data.clone())], &mut results)?;
    let expected: Vec<Val> = data.iter().chain(&data).cloned().collect();
    assert_eq!(results[0], Val::List(expected));
    Ok(())
}

#[test]
fn data_past_the_first_page_is_in_the_initial_memory() -> Result<()> {
    let (mut store, instance) = instantiate(Arc::default())?;
    let function = function(&mut store, &instance, "text")?;
    let mut results = [Val::Bool(false)];
    function.call(&mut store, &[], &mut results)?;
    assert_eq!(results[0], Val::String(text()));
    Ok(())
}

#[test]
fn a_length_from_an_import_builds_the_list() -> Result<()> {
    let (mut store, instance) = instantiate(Arc::new(AtomicU32::new(3)))?;
    let bytes = function(&mut store, &instance, "bytes")?;
    let mut results = [Val::Bool(false)];
    bytes.call(&mut store, &[], &mut results)?;
    assert_eq!(results[0], Val::List(vec![Val::U8(0); 3]));
    Ok(())
}

#[test]
fn a_size_overflowing_32_bits_traps() -> Result<()> {
    // 2^30 eight-byte elements is 8 GiB, which would wrap to 0.
    assert_traps(1 << 30, "words")
}

#[test]
fn an_allocation_past_the_address_space_traps() -> Result<()> {
    // The size fits in 32 bits, but the result's own 8 bytes are allocated
    // first, so a block this large would end past the address space.
    assert_traps(u32::MAX, "bytes")
}
