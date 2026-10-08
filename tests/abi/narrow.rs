//! Narrow leaf values where comparing with wasmtime cannot see a mistake.
//!
//! A narrow literal stored four bytes wide spills into the next field, but
//! that field's own store then overwrites the spill, since the writer stores
//! fields in address order. So the overrun shows only in the generated code.
//! A narrow signed integer read without sign extension lifts to the same
//! value, since a lift keeps only the low bits, so it shows only once the
//! factory widens it itself.

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasmparser::{Operator, Parser, Payload};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store};

const WIT: &str = r"package test:narrow;
    interface src {
      record holder { a: s8, b: u8, c: s16 }
      record bytes { a: u8, b: u8, c: u16, d: bool, e: s8 }
      get: func() -> holder;
    }
    world narrow {
      use src.{bytes};
      import src;
      export literals: func() -> bytes;
      export a-as-s32: func() -> s32;
      export a-as-s64: func() -> s64;
      export c-as-s32: func() -> s32;
      export c-as-s64: func() -> s64;
    }";

struct Narrow;

impl ComponentBuilder for Narrow {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let narrow = PackageSource::from_text(WIT)?.world("narrow")?;
        world.add_imports(narrow.imports())?;
        world.add_exports(narrow.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let result = function.result().context("returns")?.value();
        if function.name() == "literals" {
            return result.write(&ValueSpec::record([
                ("a", ValueSpec::u8(0x11)),
                ("b", ValueSpec::u8(0x22)),
                ("c", ValueSpec::u16(0x3344)),
                ("d", ValueSpec::bool(true)),
                ("e", ValueSpec::s8(-5)),
            ]));
        }
        // The holder is returned through memory, so its fields are read there.
        let holder = imports
            .interface("src")?
            .function("get")?
            .call(&[])?
            .context("get returns a holder")?;
        let (field, target) = match function.name() {
            "a-as-s32" => ("a", wit_parser::Type::S32),
            "a-as-s64" => ("a", wit_parser::Type::S64),
            "c-as-s32" => ("c", wit_parser::Type::S32),
            _ => ("c", wit_parser::Type::S64),
        };
        result.write(&ValueSpec::from(holder.field(field)?.coerce(target)?))
    }
}

fn call(name: &str) -> Result<Val> {
    let engine = Engine::default();
    let component = Component::new(&engine, build(&Narrow)?)?;
    let mut linker = Linker::<()>::new(&engine);
    linker
        .instance("test:narrow/src")?
        .func_new("get", |_, _, _, results| {
            results[0] = Val::Record(vec![
                ("a".into(), Val::S8(-1)),
                ("b".into(), Val::U8(0x7F)),
                ("c".into(), Val::S16(-2)),
            ]);
            Ok(())
        })?;
    let mut store = Store::new(&engine, ());
    let instance = linker.instantiate(&mut store, &component)?;
    let function = instance
        .get_func(&mut store, name)
        .context(name.to_string())?;
    let mut results = [Val::Bool(false)];
    function.call(&mut store, &[], &mut results)?;
    Ok(results[0].clone())
}

#[test]
fn narrow_signed_fields_in_memory_widen_with_their_sign() -> Result<()> {
    assert_eq!(call("a-as-s32")?, Val::S32(-1));
    assert_eq!(call("a-as-s64")?, Val::S64(-1));
    assert_eq!(call("c-as-s32")?, Val::S32(-2));
    assert_eq!(call("c-as-s64")?, Val::S64(-2));
    Ok(())
}

#[test]
fn narrow_literals_are_stored_at_their_own_width() -> Result<()> {
    assert_eq!(
        call("literals")?,
        Val::Record(vec![
            ("a".into(), Val::U8(0x11)),
            ("b".into(), Val::U8(0x22)),
            ("c".into(), Val::U16(0x3344)),
            ("d".into(), Val::Bool(true)),
            ("e".into(), Val::S8(-5)),
        ])
    );
    // Every field is one or two bytes, so nothing in the component should
    // store four or eight.
    let bytes = build(&Narrow)?;
    let mut wide = Vec::new();
    for payload in Parser::new(0).parse_all(&bytes) {
        if let Payload::CodeSectionEntry(body) = payload? {
            for operator in body.get_operators_reader()? {
                let operator = operator?;
                if matches!(
                    operator,
                    Operator::I32Store { .. } | Operator::I64Store { .. }
                ) {
                    wide.push(format!("{operator:?}"));
                }
            }
        }
    }
    assert!(wide.is_empty(), "wide stores: {wide:?}");
    Ok(())
}
