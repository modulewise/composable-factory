//! A factory writing a case of each variant-like kind with `write_case`, and
//! the payload through the value it returns.

use anyhow::{Context, Result, bail};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store};

const CASES_WIT: &str = r"package test:write-case;
    interface cases {
      record point { x: u32, y: u32 }
      enum color { red, green, blue }
      variant shape { dot, circle(u32) }
      found: func() -> result<point, string>;
      refused: func() -> result<point, string>;
      done: func() -> result<_, string>;
      some-point: func() -> option<point>;
      circle: func() -> shape;
      green: func() -> color;
    }
    world write-case {
      export cases;
    }";

fn point() -> ValueSpec {
    ValueSpec::record([("x", ValueSpec::u32(3)), ("y", ValueSpec::u32(4))])
}

/// Each function writes its case with `write_case`, then writes the payload
/// through the returned value if the case has one.
struct Cases;

impl ComponentBuilder for Cases {
    fn build_world(&self, world: &mut World) -> Result<()> {
        world.add_exports(PackageSource::from_text(CASES_WIT)?.interface("cases")?)
    }

    fn build_function(&self, function: &ExportedFunction, _imports: &Imports) -> Result<()> {
        let (case, payload) = match function.name() {
            "found" => ("ok", Some(point())),
            "refused" => ("err", Some(ValueSpec::string("no"))),
            "done" => ("ok", None),
            "some-point" => ("some", Some(point())),
            "circle" => ("circle", Some(ValueSpec::u32(5))),
            _ => ("green", None),
        };
        let result = function
            .result()
            .context("every function returns a value")?
            .value();
        match (result.write_case(case)?, payload) {
            (Some(value), Some(payload)) => value.write(&payload),
            (None, None) => Ok(()),
            (Some(_), None) => bail!("case '{case}' has a payload"),
            (None, Some(_)) => bail!("case '{case}' has no payload"),
        }
    }
}

#[test]
fn each_case_is_written_and_its_payload_returned() -> Result<()> {
    let engine = Engine::default();
    let component = Component::new(&engine, build(&Cases)?)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::<()>::new(&engine).instantiate(&mut store, &component)?;
    let cases = instance
        .get_export_index(&mut store, None, "test:write-case/cases")
        .context("the cases export")?;
    let point = || {
        Val::Record(vec![
            ("x".to_string(), Val::U32(3)),
            ("y".to_string(), Val::U32(4)),
        ])
    };
    for (name, expected) in [
        ("found", Val::Result(Ok(Some(Box::new(point()))))),
        (
            "refused",
            Val::Result(Err(Some(Box::new(Val::String("no".to_string()))))),
        ),
        ("done", Val::Result(Ok(None))),
        ("some-point", Val::Option(Some(Box::new(point())))),
        (
            "circle",
            Val::Variant("circle".to_string(), Some(Box::new(Val::U32(5)))),
        ),
        ("green", Val::Enum("green".to_string())),
    ] {
        let function = instance
            .get_export_index(&mut store, Some(&cases), name)
            .and_then(|index| instance.get_func(&mut store, index))
            .with_context(|| format!("the {name} function"))?;
        let mut results = [Val::Bool(false)];
        function.call(&mut store, &[], &mut results)?;
        assert_eq!(results[0], expected, "{name}");
    }
    Ok(())
}

/// Function returning `ty`, with `case` written as its result.
struct CaseOf {
    ty: &'static str,
    case: &'static str,
}

impl ComponentBuilder for CaseOf {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let wit = format!(
            "package test:case-of;
             interface case-of {{
               enum color {{ red, yellow }}
               get: func() -> {};
             }}
             world case-of-world {{ export case-of; }}",
            self.ty
        );
        world.add_exports(PackageSource::from_text(&wit)?.interface("case-of")?)
    }

    fn build_function(&self, function: &ExportedFunction, _imports: &Imports) -> Result<()> {
        function
            .result()
            .context("get returns a value")?
            .value()
            .write_case(self.case)
            .map(|_| ())
    }
}

#[test]
fn an_unknown_case_fails_the_build() {
    let error = build(&CaseOf {
        ty: "color",
        case: "orange",
    })
    .expect_err("color has no orange");
    assert!(
        format!("{error:#}").contains("write_case: no case 'orange'"),
        "{error:#}"
    );
}

#[test]
fn a_type_without_cases_fails_the_build() {
    let error = build(&CaseOf {
        ty: "u32",
        case: "ok",
    })
    .expect_err("u32 has no cases");
    assert!(
        format!("{error:#}").contains("not a variant-like type"),
        "{error:#}"
    );
}
