//! The Wasm Component being built with a factory.

use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail};
use wasm_encoder::Instruction;
use wit_parser::{Resolve, WorldId};

use crate::abi::{self, GeneratedFunction};
use crate::emitter::Emitter;
use crate::module::{Data, TypeTable};
use crate::values::{BuildContext, reserve, reserve_memory};
use crate::world::{ExportedFunction, FunctionResult, Imports, Value};

/// A component with its world, before any function bodies exist.
pub struct Component {
    resolve: Resolve,
    world: WorldId,
}

impl Component {
    pub(crate) fn new(resolve: Resolve, world: WorldId) -> Self {
        Component { resolve, world }
    }

    /// Build every exported function, calling `build_function` once for each.
    /// Each export gets a [`FunctionBuilder`] that reserves the result, hands
    /// out the two handles a factory writes against, and encodes the result
    /// delivery. A factory only sees one exported function at a time.
    ///
    /// Returns a [`Built`], which can be encoded. The function bodies are
    /// built exactly once, and nothing can be encoded before they are.
    pub(crate) fn build_functions(
        self,
        build_function: impl Fn(&ExportedFunction, &Imports) -> Result<()>,
    ) -> Result<Built> {
        let ctx = Rc::new(BuildContext::new(Rc::new(self.resolve.clone()), self.world));
        let mut generated = Vec::new();
        for entry in abi::export_entries(&self.resolve, self.world) {
            let func = abi::exported_function(
                &self.resolve,
                self.world,
                entry.interface.as_ref(),
                &entry.func,
            )?;
            let mut builder = FunctionBuilder::new(
                Rc::clone(&ctx),
                entry.param_count,
                func,
                entry.interface.as_ref(),
                entry.async_index,
            );
            // The body's first instructions, before the factory can emit.
            builder.reserve_result()?;
            build_function(&builder.function(), &builder.imports())
                .with_context(|| format!("building '{}'", entry.func))?;
            generated.push(GeneratedFunction {
                interface: entry.interface,
                func: entry.func,
                body: builder.build()?,
            });
        }
        // Capture anything the bodies interned along the way.
        let (types, data) = ctx.take_module_state();
        Ok(Built {
            resolve: self.resolve,
            world: self.world,
            types,
            data,
            generated,
        })
    }
}

/// A component whose function bodies are generated, ready to encode.
pub struct Built {
    resolve: Resolve,
    world: WorldId,
    types: TypeTable,
    data: Data,
    generated: Vec<GeneratedFunction>,
}

impl Built {
    /// Encode the generated functions into a component implementing this
    /// world. The core module is assembled and encoded first, then wrapped.
    pub(crate) fn encode(self) -> Result<Vec<u8>> {
        let core = abi::core_module(
            &self.resolve,
            self.world,
            self.generated,
            self.types,
            self.data,
        )
        .context("assembling the core module")?
        .encode();
        wrap(core, &self.resolve, self.world)
    }
}

/// Builds one exported function. It reserves the result, provides the params
/// for the implementor callback, emits result delivery, and encodes the body.
pub struct FunctionBuilder {
    ctx: Rc<BuildContext>,
    emitter: Emitter,
    /// The function being built.
    function: ExportedFunction,
    /// Position of this function's `task.return`.
    async_index: Option<usize>,
}

impl FunctionBuilder {
    /// A builder for one export body. `param_count` is the core param count,
    /// which is where the body's own locals begin.
    pub(crate) fn new(
        ctx: Rc<BuildContext>,
        param_count: u32,
        func: &wit_parser::Function,
        interface: Option<&wit_parser::WorldKey>,
        async_index: Option<usize>,
    ) -> Self {
        let emitter = Emitter::new(param_count);
        let function = ExportedFunction::new(
            Rc::clone(&ctx),
            emitter.clone(),
            interface.cloned(),
            Rc::new(func.clone()),
        );
        FunctionBuilder {
            ctx,
            emitter,
            function,
            async_index,
        }
    }

    /// Allocate this function's result, the body's first instructions.
    /// Called before the implementor's callback is invoked.
    pub(crate) fn reserve_result(&mut self) -> Result<()> {
        let Some(ty) = self.function.result_type() else {
            return Ok(());
        };
        // When the signature indicates an indirect return, the core result is
        // a pointer, so delivery requires a memory base to hand back.
        let indirect = abi::export_returns_indirectly(self.ctx.resolve(), self.function.wit());
        let slot = if indirect {
            reserve_memory(&self.ctx, &self.emitter, ty.wit())
        } else {
            reserve(&self.ctx, &self.emitter, ty.wit())?
        };
        let value = Value::new(ty, slot, self.emitter.clone());
        self.function
            .set_result(FunctionResult::new(value, indirect));
        Ok(())
    }

    /// The exported function being built.
    pub(crate) fn function(&self) -> ExportedFunction {
        self.function.clone()
    }

    /// The imports this body may call.
    pub(crate) fn imports(&self) -> Imports {
        Imports::new(Rc::clone(&self.ctx), self.emitter.clone())
    }

    /// Emit this function's result delivery, then produce the encoded body.
    ///
    /// | result | emits |
    /// |---|---|
    /// | none, sync | nothing; falling off the end is the core return |
    /// | none, async | `task.return` with no values, so the task completes |
    /// | some, sync | the flats on the stack, or the retarea pointer |
    /// | some, async | flats-or-pointer, then `task.return` |
    pub(crate) fn build(self) -> Result<wasm_encoder::Function> {
        self.deliver_result()?;
        self.emitter.encode()
    }

    fn deliver_result(&self) -> Result<()> {
        let is_async = self.function.wit().kind.is_async();
        let Some(result) = self.function.result() else {
            if is_async {
                self.emit_task_return()?;
            }
            return Ok(());
        };
        let value = result.value();

        // A sync body may deliver its own result instead of writing the one
        // reserved for it. An async body cannot: `task.return` is the only
        // delivery, and it reads the reserved value.
        if !value.was_written() {
            if is_async {
                bail!(
                    "'{}' is async, so its result must be written into \
                     the reserved value for `task.return` to deliver",
                    self.function.name()
                );
            }
            return Ok(());
        }
        if result.indirect() {
            let base = value
                .base()
                .expect("an indirect result is reserved in memory");
            self.emitter.emit(Instruction::LocalGet(base));
        } else {
            value.push()?;
        }
        if is_async {
            self.emit_task_return()?;
        }
        Ok(())
    }

    fn emit_task_return(&self) -> Result<()> {
        let async_index = self
            .async_index
            .ok_or_else(|| anyhow!("an async function has no async index"))?;
        let index = abi::task_return_index(self.ctx.resolve(), self.ctx.world(), async_index);
        self.emitter.emit(Instruction::Call(index));
        Ok(())
    }
}

/// Wrap a core module into a component and validate.
fn wrap(mut core: Vec<u8>, resolve: &Resolve, world: WorldId) -> Result<Vec<u8>> {
    wit_component::embed_component_metadata(
        &mut core,
        resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )
    .context("embedding the world metadata")?;

    let component = wit_component::ComponentEncoder::default()
        .module(&core)
        .context("reading the core module")?
        .validate(true)
        .encode()
        .context("encoding the component")?;

    wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
        .validate_all(&component)
        .context("validating the encoded component")?;

    Ok(component)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::ValueSpec;
    use anyhow::bail;

    fn world(wit: &str) -> (Resolve, WorldId) {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("test.wit", wit).expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");
        (resolve, world)
    }

    fn validate(component: &[u8]) {
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(component)
            .expect("the encoded component must be valid");
    }

    #[test]
    fn encodes_a_component_for_its_world() {
        let (resolve, w) = world(r"package test:one; world w { export run: func(); }");
        let component = Component::new(resolve, w)
            .build_functions(|_, _| Ok(()))
            .expect("build");
        validate(&component.encode().expect("encode"));
    }

    #[test]
    fn encodes_a_component_with_no_exports() {
        let (resolve, w) = world(r"package test:none; world w {}");
        let component = Component::new(resolve, w)
            .build_functions(|_, _| unreachable!("nothing is exported"))
            .expect("build");
        validate(&component.encode().expect("encode"));
    }

    #[test]
    fn every_added_function_is_encoded() {
        let (resolve, w) = world(
            r"package test:multi;
              world w { export first: func(); export second: func(); }",
        );
        let component = Component::new(resolve, w)
            .build_functions(|_, _| Ok(()))
            .expect("build");
        // The encoder rejects a module leaving a declared export
        // unimplemented, so a valid component means both bodies arrived.
        validate(&component.encode().expect("encode"));
    }

    #[test]
    fn exported_interface_is_encoded() {
        let (resolve, w) = world(
            r"package test:named;
              interface greeter { greet: func(); }
              world w { export greeter; }",
        );
        let component = Component::new(resolve, w)
            .build_functions(|_, _| Ok(()))
            .expect("build");
        validate(&component.encode().expect("encode"));
    }

    /// Build every export of `wit` with `build_function`, and validate.
    fn build(wit: &str, build_function: impl Fn(&ExportedFunction, &Imports) -> Result<()>) {
        let (resolve, w) = world(wit);
        let component = Component::new(resolve, w)
            .build_functions(build_function)
            .expect("build");
        validate(&component.encode().expect("encode"));
    }

    #[test]
    fn an_export_with_no_result_needs_no_delivery() {
        build(
            r"package test:e2enoresult; world w { export run: func(); }",
            |_, _| Ok(()),
        );
    }

    #[test]
    fn an_async_export_delivers_through_task_return() {
        // An end-to-end check that the `[async-lift-stackful]` export and
        // `[task-return]` import names match what the encoder expects.
        build(
            r"package test:e2easync; world w { export answer: async func() -> u32; }",
            |function, _| {
                function
                    .result()
                    .expect("a declared result")
                    .value()
                    .write(&ValueSpec::u32(7))
            },
        );
    }

    #[test]
    fn an_async_export_must_write_its_reserved_result() {
        let (resolve, w) =
            world(r"package test:e2easyncbare; world w { export answer: async func() -> u32; }");
        let Err(error) = Component::new(resolve, w).build_functions(|_, _| Ok(())) else {
            panic!("an async result must be written");
        };
        let text = format!("{error:#}");
        assert!(text.contains("task.return"), "{text}");
    }

    #[test]
    fn a_flat_result_is_delivered_on_the_stack() {
        build(
            r"package test:e2eflat; world w { export answer: func() -> u32; }",
            |function, _| {
                function
                    .result()
                    .expect("a declared result")
                    .value()
                    .write(&ValueSpec::u32(42))
            },
        );
    }

    #[test]
    fn an_indirect_result_is_delivered_as_a_pointer() {
        // A record of two fields returns through memory and delivery hands
        // back the retarea pointer.
        build(
            r"package test:e2eindirect;
              world w {
                record pair { a: u32, b: u64 }
                export make: func() -> pair;
              }",
            |function, _| {
                function
                    .result()
                    .expect("a declared result")
                    .value()
                    .write(&ValueSpec::record([
                        ("a", ValueSpec::u32(1)),
                        ("b", ValueSpec::u64(2)),
                    ]))
            },
        );
    }

    #[test]
    fn a_string_result_reaches_the_data_segment() {
        build(
            r"package test:e2estr; world w { export greet: func() -> string; }",
            |function, _| {
                function
                    .result()
                    .expect("a declared result")
                    .value()
                    .write(&ValueSpec::string("hello"))
            },
        );
    }

    #[test]
    fn a_param_is_received_and_returned() {
        build(
            r"package test:e2eparam;
              world w { export echo: func(n: u32) -> u32; }",
            |function, _| {
                let n = function.param("n")?.receive()?;
                function
                    .result()
                    .expect("a declared result")
                    .value()
                    .write(&ValueSpec::from(n))
            },
        );
    }

    #[test]
    fn an_import_is_called_with_a_passed_argument() {
        build(
            r"package test:e2ecall;
              interface logging { log: func(message: string); }
              world w {
                import logging;
                export run: func();
              }",
            |_, imports| {
                let log = imports.interface("logging")?.function("log")?;
                let message = log.param("message")?.value()?;
                message.write(&ValueSpec::string("building"))?;
                log.call(&[message])?;
                Ok(())
            },
        );
    }

    #[test]
    fn a_result_can_forward_an_imported_result() {
        build(
            r"package test:e2eforward;
              interface source { get: func() -> u32; }
              world w {
                import source;
                export forward: func() -> u32;
              }",
            |function, imports| {
                let got = imports
                    .interface("source")?
                    .function("get")?
                    .call(&[])?
                    .expect("a declared result");
                function
                    .result()
                    .expect("a declared result")
                    .value()
                    .write(&ValueSpec::from(got))
            },
        );
    }

    #[test]
    fn every_export_gets_its_own_body() {
        // The loop runs once per export, and each body is independent.
        let calls = std::cell::RefCell::new(Vec::new());
        build(
            r"package test:e2emulti;
              world w {
                export first: func() -> u32;
                export second: func() -> u32;
              }",
            |function, _| {
                calls.borrow_mut().push(function.name().to_string());
                function
                    .result()
                    .expect("a declared result")
                    .value()
                    .write(&ValueSpec::u32(1))
            },
        );
        assert_eq!(calls.into_inner(), ["first", "second"]);
    }

    #[test]
    fn a_factory_error_names_the_function_it_came_from() {
        let (resolve, w) = world(
            r"package test:e2eerr;
              world w { export run: func() -> u32; }",
        );
        let Err(error) =
            Component::new(resolve, w).build_functions(|_, _| bail!("factory gave up"))
        else {
            panic!("the factory's error must surface");
        };
        let text = format!("{error:#}");
        assert!(text.contains("run"), "names the export: {text}");
        assert!(text.contains("gave up"), "records the cause: {text}");
    }

    #[test]
    fn an_unwritten_result_leaves_delivery_to_the_body() {
        // Result must be left on stack if the reserved value was not written.
        build(
            r"package test:e2eunwritten; world w { export answer: func() -> u32; }",
            |function, _| {
                function.body().emit(Instruction::I32Const(42));
                Ok(())
            },
        );
    }
}
