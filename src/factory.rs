//! The component-building factory and the implementor's API.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use wit_parser::{InterfaceId, Resolve, TypeId, WorldId, WorldItem, WorldKey};

use crate::component::Component;
use crate::wit::Selection;
use crate::world::{ExportedFunction, Imports};

/// An implementor's contribution for building a component, with two callbacks.
/// The first declares the component's world, and the second is called once for
/// each exported function in that world.
pub trait ComponentBuilder {
    /// Declare the component's world by selecting from WIT sources
    /// and calling [`World::add_imports`] and/or [`World::add_exports`].
    fn build_world(&self, world: &mut World) -> Result<()>;

    /// Build a function's body. Called once per exported function declared in
    /// the world. Also provides the callable imports declared in the world.
    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()>;
}

/// Builds and encodes a component, delegating to the implementor's callbacks.
pub fn build(factory: &dyn ComponentBuilder) -> Result<Vec<u8>> {
    let mut builder = WorldBuilder::new();
    factory.build_world(&mut builder.world())?;
    let (resolve, world) = builder.build()?;
    Component::new(resolve, world)
        .build_functions(|function, imports| factory.build_function(function, imports))?
        .encode()
}

/// The world being declared for the component being built.
pub struct World<'b> {
    builder: &'b mut WorldBuilder,
}

impl World<'_> {
    /// Add a selection to the world's imports.
    pub fn add_imports(&mut self, selection: Selection) -> Result<()> {
        self.builder.add(selection, Role::Import)
    }

    /// Add a selection to the world's exports.
    pub fn add_exports(&mut self, selection: Selection) -> Result<()> {
        self.builder.add(selection, Role::Export)
    }
}

/// Which side of the component boundary a member sits on.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Import,
    Export,
}

/// Assembles one world from the selections made in `build_world`.
struct WorldBuilder {
    /// The one merged superset every selection is absorbed into.
    resolve: Resolve,
    imports: Vec<Member>,
    exports: Vec<Member>,
}

/// One recorded membership, by its identity in the superset.
enum Member {
    /// An interface, which brings its own functions and types.
    Interface(InterfaceId),
    /// A world-level function: its name and declaring world.
    Function { world: WorldId, name: String },
}

impl WorldBuilder {
    fn new() -> Self {
        WorldBuilder {
            resolve: Resolve::new(),
            imports: Vec::new(),
            exports: Vec::new(),
        }
    }

    /// The factory-facing view, for the span of one `build_world`.
    fn world(&mut self) -> World<'_> {
        World { builder: self }
    }

    /// Merge a selection's definitions into the superset and record what it
    /// selected as members of `role`.
    fn add(&mut self, selection: Selection, role: Role) -> Result<()> {
        let remap = self.resolve.merge(selection.resolve.clone())?;
        for id in &selection.interfaces {
            let id = remap
                .map_interface(*id, wit_parser::Span::default())
                .map_err(|error| anyhow!("{error}"))?;
            self.push(role, Member::Interface(id));
        }
        // World-level functions are rooted in the world that declared them.
        let Some(source) = selection.world else {
            return Ok(());
        };
        let world = remap
            .map_world(source, wit_parser::Span::default())
            .map_err(|error| anyhow!("{error}"))?;
        for name in selection.functions {
            self.push(role, Member::Function { world, name });
        }
        Ok(())
    }

    /// Record a member. Duplicates are collapsed when the world is built,
    /// where each member becomes an entry keyed by its identity.
    fn push(&mut self, role: Role, member: Member) {
        match role {
            Role::Import => self.imports.push(member),
            Role::Export => self.exports.push(member),
        }
    }

    /// Build a world with the selected members.
    fn build(mut self) -> Result<(Resolve, WorldId)> {
        use wit_parser::{Package, PackageName, Stability, World as WitWorld};

        let package_name = PackageName {
            namespace: "factory".into(),
            name: "component".into(),
            version: None,
        };
        let package = self.resolve.packages.alloc(Package {
            name: package_name.clone(),
            docs: Default::default(),
            interfaces: Default::default(),
            worlds: Default::default(),
        });
        self.resolve.package_names.insert(package_name, package);

        let world = self.resolve.worlds.alloc(WitWorld {
            name: WORLD_NAME.into(),
            imports: Default::default(),
            exports: Default::default(),
            package: Some(package),
            docs: Default::default(),
            stability: Stability::Unknown,
            includes: Vec::new(),
            span: wit_parser::Span::default(),
        });
        self.resolve.packages[package]
            .worlds
            .insert(WORLD_NAME.into(), world);

        let mut imports = indexmap::IndexMap::new();
        let mut exports = indexmap::IndexMap::new();
        for (members, into) in [(&self.imports, &mut imports), (&self.exports, &mut exports)] {
            for member in members {
                match member {
                    Member::Interface(id) => {
                        into.insert(
                            WorldKey::Interface(*id),
                            WorldItem::Interface {
                                id: *id,
                                stability: Stability::Unknown,
                                span: wit_parser::Span::default(),
                                docs: Default::default(),
                                external_id: None,
                            },
                        );
                    }
                    Member::Function { world, name } => {
                        let source = &self.resolve.worlds[*world];
                        let func = source
                            .imports
                            .values()
                            .chain(source.exports.values())
                            .find_map(|item| match item {
                                WorldItem::Function(func) if &func.name == name => {
                                    Some(func.clone())
                                }
                                _ => None,
                            })
                            .ok_or_else(|| {
                                anyhow!("world-level function '{name}' is not in its source world")
                            })?;
                        into.insert(WorldKey::Name(name.clone()), WorldItem::Function(func));
                    }
                }
            }
        }

        self.localize_use_aliases(world, &mut imports, &mut exports);
        let declared = &mut self.resolve.worlds[world];
        declared.imports = imports;
        declared.exports = exports;
        // Merging even an empty Resolve fires `elaborate_world`, which
        // back-fills the provider imports every member's `use`d types need.
        self.resolve.merge(Resolve::default())?;
        Ok((self.resolve, world))
    }

    /// Give the authored world its own `use` aliases for the foreign types its
    /// world-level functions reference. Each world-level function must
    /// reference a world-owned type, but ours are cloned from source worlds
    /// and still reference those worlds' aliases. Interface members do not
    /// require localizing since they reference interface-owned types.
    fn localize_use_aliases(
        &mut self,
        world: WorldId,
        imports: &mut indexmap::IndexMap<WorldKey, WorldItem>,
        exports: &mut indexmap::IndexMap<WorldKey, WorldItem>,
    ) {
        use wit_parser::TypeOwner;

        // Gather every foreign alias a world-level function reaches: a named
        // type owned by an interface rather than by a world.
        let mut foreign: Vec<TypeId> = Vec::new();
        for item in imports.values().chain(exports.values()) {
            let WorldItem::Function(func) = item else {
                continue;
            };
            let mut live = wit_parser::LiveTypes::default();
            live.add_func(&self.resolve, func);
            for ty in live.iter() {
                if self.resolve.type_interface_dep(ty).is_some() && !foreign.contains(&ty) {
                    foreign.push(ty);
                }
            }
        }
        if foreign.is_empty() {
            return;
        }

        // Allocate new type definitions with the same name and kind, but with
        // the new world as owner. Insert each into the world's imports.
        let mut localized: HashMap<TypeId, TypeId> = HashMap::new();
        for source in foreign {
            let (name, kind) = {
                let declared = &self.resolve.types[source];
                (declared.name.clone(), declared.kind.clone())
            };
            let local = self.resolve.types.alloc(wit_parser::TypeDef {
                name: name.clone(),
                kind,
                owner: TypeOwner::World(world),
                docs: Default::default(),
                stability: Default::default(),
                span: wit_parser::Span::default(),
                external_id: None,
            });
            imports.insert(
                WorldKey::Name(name.expect("a `use` alias is always named")),
                WorldItem::Type {
                    id: local,
                    span: wit_parser::Span::default(),
                },
            );
            localized.insert(source, local);
        }

        // Repoint any function params or results that use foreign aliases.
        for group in [imports, exports] {
            for item in group.values_mut() {
                let WorldItem::Function(func) = item else {
                    continue;
                };
                for param in func.params.iter_mut() {
                    param.ty = self.repoint(param.ty, &localized);
                }
                if let Some(result) = func.result {
                    func.result = Some(self.repoint(result, &localized));
                }
            }
        }
    }

    /// Redirect every foreign alias in `ty` to point to its localized
    /// replacement. If a named type is a foreign alias, it is swapped, else it
    /// is returned unchanged. Recurses into anonymous composites and clones
    /// rather than mutates them, since they are shared with the source world.
    fn repoint(
        &mut self,
        ty: wit_parser::Type,
        localized: &HashMap<TypeId, TypeId>,
    ) -> wit_parser::Type {
        use wit_parser::{Handle, Type, TypeDefKind};

        let Type::Id(id) = ty else { return ty };
        if let Some(&local) = localized.get(&id) {
            return Type::Id(local);
        }
        if self.resolve.types[id].name.is_some() {
            return ty;
        }
        let mut kind = self.resolve.types[id].kind.clone();
        match &mut kind {
            TypeDefKind::List(inner)
            | TypeDefKind::Option(inner)
            | TypeDefKind::FixedLengthList(inner, _) => {
                *inner = self.repoint(*inner, localized);
            }
            TypeDefKind::Map(key, value) => {
                *key = self.repoint(*key, localized);
                *value = self.repoint(*value, localized);
            }
            TypeDefKind::Tuple(tuple) => {
                for member in tuple.types.iter_mut() {
                    *member = self.repoint(*member, localized);
                }
            }
            TypeDefKind::Record(record) => {
                for field in record.fields.iter_mut() {
                    field.ty = self.repoint(field.ty, localized);
                }
            }
            TypeDefKind::Variant(variant) => {
                for case in variant.cases.iter_mut() {
                    if let Some(payload) = case.ty {
                        case.ty = Some(self.repoint(payload, localized));
                    }
                }
            }
            TypeDefKind::Result(result) => {
                if let Some(ok) = result.ok {
                    result.ok = Some(self.repoint(ok, localized));
                }
                if let Some(err) = result.err {
                    result.err = Some(self.repoint(err, localized));
                }
            }
            TypeDefKind::Handle(Handle::Own(resource) | Handle::Borrow(resource)) => {
                if let Some(&local) = localized.get(resource) {
                    *resource = local;
                }
            }
            TypeDefKind::Future(payload) | TypeDefKind::Stream(payload) => {
                if let Some(inner) = payload {
                    *inner = self.repoint(*inner, localized);
                }
            }
            TypeDefKind::Type(inner) => {
                *inner = self.repoint(*inner, localized);
            }
            TypeDefKind::Resource
            | TypeDefKind::Flags(_)
            | TypeDefKind::Enum(_)
            | TypeDefKind::Unknown => {}
        }
        let declared = self.resolve.types[id].clone();
        let local = self
            .resolve
            .types
            .alloc(wit_parser::TypeDef { kind, ..declared });
        Type::Id(local)
    }
}

/// The name the authored world is declared under.
const WORLD_NAME: &str = "built-world";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::ValueSpec;
    use crate::wit::PackageSource;

    /// A factory declaring its world from WIT text and writing one body.
    struct Factory {
        wit: &'static str,
        declare: fn(&mut World, PackageSource) -> Result<()>,
        body: fn(&ExportedFunction, &Imports) -> Result<()>,
    }

    impl ComponentBuilder for Factory {
        fn build_world(&self, world: &mut World) -> Result<()> {
            (self.declare)(world, PackageSource::from_text(self.wit)?)
        }

        fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
            (self.body)(function, imports)
        }
    }

    fn validate(component: &[u8]) {
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(component)
            .expect("the built component must be valid");
    }

    const WIT: &str = r"package test:factory;
        interface greeter { greet: func() -> string; }
        interface logger { log: func(message: string); }
        world w {
          import logger;
          export greeter;
          export run: func();
        }";

    #[test]
    fn a_world_of_one_exported_interface_builds() {
        let bytes = build(&Factory {
            wit: WIT,
            declare: |world, package| world.add_exports(package.interface("greeter")?),
            body: |function, _| {
                function
                    .result()
                    .expect("greet returns a string")
                    .value()
                    .write(&ValueSpec::string("hello"))
            },
        })
        .expect("build");
        validate(&bytes);
    }

    #[test]
    fn an_imported_interface_is_callable_from_an_export() {
        let bytes = build(&Factory {
            wit: WIT,
            declare: |world, package| {
                world.add_imports(package.interface("logger")?)?;
                world.add_exports(package.interface("greeter")?)
            },
            body: |function, imports| {
                let log = imports.interface("logger")?.function("log")?;
                let message = log.param("message")?.value()?;
                message.write(&ValueSpec::string("greeting"))?;
                log.call(&[message])?;
                function
                    .result()
                    .expect("greet returns a string")
                    .value()
                    .write(&ValueSpec::string("hello"))
            },
        })
        .expect("build");
        validate(&bytes);
    }

    #[test]
    fn a_world_level_export_builds() {
        let bytes = build(&Factory {
            wit: WIT,
            declare: |world, package| {
                let source = package.world("w")?;
                world.add_exports(source.function("run")?)
            },
            body: |_, _| Ok(()),
        })
        .expect("build");
        validate(&bytes);
    }

    #[test]
    fn the_same_interface_may_be_imported_and_exported() {
        // A pass-through component: the same interface on both sides.
        let bytes = build(&Factory {
            wit: WIT,
            declare: |world, package| {
                world.add_imports(package.interface("greeter")?)?;
                world.add_exports(package.interface("greeter")?)
            },
            body: |function, imports| {
                let forwarded = imports
                    .interface("greeter")?
                    .function("greet")?
                    .call(&[])?
                    .expect("greet returns a string");
                function
                    .result()
                    .expect("greet returns a string")
                    .value()
                    .write(&ValueSpec::from(forwarded))
            },
        })
        .expect("build");
        validate(&bytes);
    }

    #[test]
    fn adding_the_same_interface_twice_records_it_once() {
        let bytes = build(&Factory {
            wit: WIT,
            declare: |world, package| {
                world.add_exports(package.interface("greeter")?)?;
                world.add_exports(package.interface("greeter")?)
            },
            body: |function, _| {
                function
                    .result()
                    .expect("greet returns a string")
                    .value()
                    .write(&ValueSpec::string("hello"))
            },
        })
        .expect("build");
        validate(&bytes);
    }

    #[test]
    fn adding_the_same_world_level_function_twice_records_it_once() {
        let bytes = build(&Factory {
            wit: r"package test:dupfunc;
                   world target { export run: func(); }",
            declare: |world, package| {
                let target = package.world("target")?;
                world.add_exports(target.function("run")?)?;
                world.add_exports(target.function("run")?)
            },
            body: |_, _| Ok(()),
        })
        .expect("build");
        validate(&bytes);
    }

    #[test]
    fn a_factory_error_in_build_world_surfaces() {
        let Err(error) = build(&Factory {
            wit: WIT,
            declare: |_, package| {
                package.interface("absent")?;
                Ok(())
            },
            body: |_, _| Ok(()),
        }) else {
            panic!("selecting an undeclared interface must fail");
        };
        assert!(format!("{error:#}").contains("absent"), "{error:#}");
    }

    #[test]
    fn a_nested_variant_reconciles_into_its_joined_slots() {
        // A variant's payload slots are widened to fit every case, so writing
        // a narrower case needs a bitcast. Nesting an `option<v>` means the
        // inner variant's slots were declared by the outer reservation, and
        // the cases below differ in both flat width and type.
        let bytes = build(&Factory {
            wit: r"package test:flatvariant;
                   interface sink {
                     variant v {
                       text(string),
                       wide(s64),
                       unsigned(u64),
                       single(f32),
                       narrow(bool),
                       empty,
                     }
                     accept: func(x: option<v>);
                   }
                   world target { import sink; export run: func(); }",
            declare: |world, package| {
                let target = package.world("target")?;
                world.add_imports(target.interface("sink")?)?;
                world.add_exports(target.function("run")?)
            },
            body: |_, imports| {
                let accept = imports.interface("sink")?.function("accept")?;
                for spec in [
                    ValueSpec::some(ValueSpec::variant("text", ValueSpec::string("hi"))),
                    ValueSpec::some(ValueSpec::variant("narrow", ValueSpec::bool(true))),
                    ValueSpec::some(ValueSpec::variant("wide", ValueSpec::s64(-9_000_000_000))),
                    ValueSpec::some(ValueSpec::variant("unsigned", ValueSpec::u64(42))),
                    ValueSpec::some(ValueSpec::variant("single", ValueSpec::f32(1.5))),
                    ValueSpec::some(ValueSpec::variant_unit("empty")),
                    ValueSpec::none(),
                ] {
                    let arg = accept.param("x")?.value()?;
                    arg.write(&spec)?;
                    accept.call(&[arg])?;
                }
                Ok(())
            },
        })
        .expect("every case must reconcile into the joined slot");
        validate(&bytes);
    }

    #[test]
    fn a_joined_slot_reconciles_lists_and_flags_as_variant_cases() {
        // A list writes a pointer and a length; flags write their bitset
        // words. Neither goes through `push_scalar`, but both land in joined
        // slots a wider sibling declared, so both still need reconciling.
        let bytes = build(&Factory {
            wit: r"package test:nonscalars;
                   interface sink {
                     flags perms { read, write }
                     variant v { data(list<u8>), grants(perms), wide(s64) }
                     accept: func(x: v);
                   }
                   world target { import sink; export run: func(); }",
            declare: |world, package| {
                let target = package.world("target")?;
                world.add_imports(target.interface("sink")?)?;
                world.add_exports(target.function("run")?)
            },
            body: |_, imports| {
                let accept = imports.interface("sink")?.function("accept")?;
                for spec in [
                    ValueSpec::variant("data", ValueSpec::list([ValueSpec::u8(1)])),
                    ValueSpec::variant("grants", ValueSpec::flags(["read"])),
                    ValueSpec::variant("wide", ValueSpec::s64(-1)),
                ] {
                    let arg = accept.param("x")?.value()?;
                    arg.write(&spec)?;
                    accept.call(&[arg])?;
                }
                Ok(())
            },
        })
        .expect("list and flags cases must reconcile into the joined slot");
        validate(&bytes);
    }

    #[test]
    fn a_produced_list_widens_into_its_joined_payload_slot() {
        // `write_with` builds into a joined slot, a path the spec never
        // reaches. The `f64` case joins with the list's i32 pointer to make
        // the first payload slot an i64, so the pointer widens into it.
        use crate::world::{Value, WriteVisitor};

        // Answers the walk's two runtime queries through real imports.
        struct Source {
            imports: Imports,
        }

        impl WriteVisitor for Source {
            fn length(&mut self) -> Result<Value> {
                Ok(self
                    .imports
                    .interface("source")?
                    .function("length")?
                    .call(&[])?
                    .expect("length returns a u32"))
            }

            fn case_index(&mut self, names: &[&str]) -> Result<Value> {
                let chooser = self.imports.interface("source")?.function("case-index")?;
                let argument = chooser.param("names")?.value()?;
                argument.write(&ValueSpec::list(
                    names.iter().map(|name| ValueSpec::string(*name)),
                ))?;
                Ok(chooser
                    .call(&[argument])?
                    .expect("case-index returns a u32"))
            }

            fn on_u32(&mut self) -> Result<ValueSpec> {
                Ok(ValueSpec::u32(0))
            }

            fn on_f64(&mut self) -> Result<ValueSpec> {
                Ok(ValueSpec::f64(0.0))
            }
        }

        struct Producing;

        impl ComponentBuilder for Producing {
            fn build_world(&self, world: &mut World) -> Result<()> {
                let target = PackageSource::from_text(
                    r"package test:producejoined;
                      interface sink {
                        variant joined { data(list<u32>), wide(f64), count(u32) }
                        accept: func(v: joined);
                      }
                      interface source {
                        length: func() -> u32;
                        case-index: func(names: list<string>) -> u32;
                      }
                      world target {
                        import sink;
                        import source;
                        export run: func();
                      }",
                )?
                .world("target")?;
                world.add_imports(target.interface("sink")?)?;
                world.add_imports(target.interface("source")?)?;
                world.add_exports(target.function("run")?)
            }

            fn build_function(&self, _: &ExportedFunction, imports: &Imports) -> Result<()> {
                let accept = imports.interface("sink")?.function("accept")?;
                let argument = accept.param("v")?.value()?;
                argument.write_with(&mut Source {
                    imports: imports.clone(),
                })?;
                accept.call(&[argument])?;
                Ok(())
            }
        }

        let bytes = build(&Producing).expect("a produced list must reconcile into a joined slot");
        validate(&bytes);
    }

    #[test]
    fn a_received_value_copies_into_a_flat_record_field() {
        // A received param is a source, so writing it into a field copies its
        // flats into the field's locals rather than authoring a value.
        let bytes = build(&Factory {
            wit: r"package test:sourcefield;
                   interface holding {
                     record holder { only: u32 }
                     wrap: func(n: u32) -> holder;
                   }
                   world target { export holding; }",
            declare: |world, package| {
                let target = package.world("target")?;
                world.add_exports(target.exports())
            },
            body: |function, _| {
                let received = function.params()[0].receive()?;
                function
                    .result()
                    .expect("wrap returns a holder")
                    .value()
                    .write(&ValueSpec::record([("only", received)]))
            },
        })
        .expect("a received value must copy into a flat record field");
        validate(&bytes);
    }

    #[test]
    fn a_received_value_copies_into_an_indirect_record_field() {
        // Two fields put the record past the one-flat result limit, so it is
        // built in memory and the copy stores instead of setting locals.
        let bytes = build(&Factory {
            wit: r"package test:sourcefieldmem;
                   interface holding {
                     record holder { first: u32, second: u64 }
                     wrap: func(a: u32, b: u64) -> holder;
                   }
                   world target { export holding; }",
            declare: |world, package| {
                let target = package.world("target")?;
                world.add_exports(target.exports())
            },
            body: |function, _| {
                let first = function.params()[0].receive()?;
                let second = function.params()[1].receive()?;
                function
                    .result()
                    .expect("wrap returns a holder")
                    .value()
                    .write(&ValueSpec::record([("first", first), ("second", second)]))
            },
        })
        .expect("a received value must copy into an indirect record field");
        validate(&bytes);
    }

    #[test]
    fn a_world_level_resource_gets_its_drop_builtin() {
        // A resource `use`d at world level is owned by the interface that
        // declares it, not by the world that names it. Its drop builtin
        // belongs to that interface, so resolving the owner is what makes the
        // drop callable.
        let bytes = build(&Factory {
            wit: r"package test:worldres;
                   interface res { resource handle; }
                   world target {
                     use res.{handle};
                     import consume: func(h: handle);
                   }",
            declare: |world, package| {
                let target = package.world("target")?;
                // The same function on both sides: exported so there is a body
                // to build, imported so the resource comes along.
                world.add_exports(target.function("consume")?)?;
                world.add_imports(target.function("consume")?)
            },
            body: |function, _| function.params()[0].receive()?.drop(),
        })
        .expect("a world-level resource must build");
        validate(&bytes);
    }

    #[test]
    fn a_use_reached_through_a_composite_is_localized() {
        // A `use`d type the function reaches through a composite must be
        // localized there too, not only where it is named directly.
        let bytes = build(&Factory {
            wit: r"package test:usenested;
                   interface shapes { record point { x: u32, y: u32 } }
                   world target {
                     use shapes.{point};
                     import plot: func(points: list<point>);
                   }",
            declare: |world, package| {
                let target = package.world("target")?;
                world.add_exports(target.function("plot")?)?;
                world.add_imports(target.function("plot")?)
            },
            body: |_, _| Ok(()),
        })
        .expect("a `use` inside a composite must localize");
        validate(&bytes);
    }
}
