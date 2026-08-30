//! WIT support for factory implementations.

use anyhow::{Result, anyhow, bail};
use wit_parser::{InterfaceId, Resolve, WorldItem};

/// Types of WIT source for use within a factory.
pub enum WitSource {
    Package(PackageSource),
    World(WorldSource),
}

/// A [`Resolve`] rooted at a package, which may declare any number of worlds.
/// Narrow to one with [`PackageSource::world`], or name definitions directly.
pub struct PackageSource {
    resolve: Resolve,
    package: wit_parser::PackageId,
}

/// A [`Resolve`] narrowed to one world, from a component's bytes or from a
/// named world within a package.
pub struct WorldSource {
    resolve: Resolve,
    world: wit_parser::WorldId,
}

impl WitSource {
    /// Decode bytes into the source type they describe. A wasm file decodes to
    /// a component world or a WIT package, anything else is read as WIT text.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if !bytes.starts_with(b"\0asm") {
            let text = std::str::from_utf8(bytes)
                .map_err(|error| anyhow!("bytes are neither wasm nor UTF-8 WIT text: {error}"))?;
            return Ok(WitSource::Package(PackageSource::from_text(text)?));
        }
        match wit_parser::decoding::decode(bytes)? {
            wit_parser::decoding::DecodedWasm::Component(resolve, world) => {
                Ok(WitSource::World(WorldSource { resolve, world }))
            }
            wit_parser::decoding::DecodedWasm::WitPackage(resolve, package) => {
                Ok(WitSource::Package(PackageSource { resolve, package }))
            }
        }
    }
}

impl PackageSource {
    /// Parse a package from WIT text.
    pub fn from_text(wit: &str) -> Result<Self> {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("source.wit", wit)?;
        Ok(PackageSource { resolve, package })
    }

    /// Decode a package from an encoded WIT package.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        match WitSource::from_bytes(bytes)? {
            WitSource::Package(source) => Ok(source),
            WitSource::World(_) => bail!("these bytes are a component, not a WIT package"),
        }
    }

    /// Narrow this package to one of its worlds.
    pub fn world(self, name: &str) -> Result<WorldSource> {
        let world = self
            .resolve
            .select_world(&[self.package], Some(name))
            .map_err(|error| anyhow!("no world '{name}' in this package: {error}"))?;
        Ok(WorldSource {
            resolve: self.resolve,
            world,
        })
    }

    /// One interface by short name or qualified `pkg:ns/name`.
    pub fn interface(&self, name: &str) -> Result<Selection<'_>> {
        let ids = self.resolve.interfaces.iter().map(|(id, _)| id);
        let id = find_one(&self.resolve, ids, name)?;
        Ok(Selection::of_interface(&self.resolve, id))
    }
}

impl WorldSource {
    /// Decode a component's bytes into the world it implements.
    pub fn from_component(bytes: &[u8]) -> Result<Self> {
        match WitSource::from_bytes(bytes)? {
            WitSource::World(source) => Ok(source),
            WitSource::Package(_) => bail!("these bytes are a WIT package, not a component"),
        }
    }

    /// Everything this world exports.
    pub fn exports(&self) -> Selection<'_> {
        self.by_role(self.resolve.worlds[self.world].exports.values())
    }

    /// Everything this world imports.
    pub fn imports(&self) -> Selection<'_> {
        self.by_role(self.resolve.worlds[self.world].imports.values())
    }

    /// One interface by short name or qualified `pkg:ns/name`, from either role.
    pub fn interface(&self, name: &str) -> Result<Selection<'_>> {
        let ids = self.items().filter_map(|item| match item {
            WorldItem::Interface { id, .. } => Some(*id),
            _ => None,
        });
        let id = find_one(&self.resolve, ids, name)?;
        Ok(Selection::of_interface(&self.resolve, id))
    }

    /// One world-level function by name, from either role.
    pub fn function(&self, name: &str) -> Result<Selection<'_>> {
        let declared = self
            .items()
            .any(|item| matches!(item, WorldItem::Function(func) if func.name == name));
        if !declared {
            bail!("no world-level function '{name}' in this world");
        }
        Ok(Selection {
            resolve: &self.resolve,
            interfaces: Vec::new(),
            functions: vec![name.to_string()],
            world: Some(self.world),
        })
    }

    // Every item this world declares, in either role.
    fn items(&self) -> impl Iterator<Item = &WorldItem> {
        let world = &self.resolve.worlds[self.world];
        world.imports.values().chain(world.exports.values())
    }

    // Everything in one role, as a selection.
    fn by_role<'a>(&'a self, items: impl Iterator<Item = &'a WorldItem>) -> Selection<'a> {
        let mut selection = Selection::empty(&self.resolve);
        for item in items {
            match item {
                WorldItem::Interface { id, .. } => selection.interfaces.push(*id),
                WorldItem::Function(func) => {
                    selection.functions.push(func.name.clone());
                    // A world-level function is rooted in the world that
                    // declares it, which is where it will be looked up.
                    selection.world = Some(self.world);
                }
                // Types ride along with whatever references them, and are never
                // selected on their own.
                WorldItem::Type { .. } => {}
            }
        }
        selection
    }
}

/// The one interface `name` selects, by short name or qualified `pkg:ns/name`.
/// Errors if there is not exactly one match.
fn find_one(
    resolve: &Resolve,
    ids: impl Iterator<Item = InterfaceId>,
    name: &str,
) -> Result<InterfaceId> {
    let mut matched = ids.filter(|id| {
        resolve.interfaces[*id].name.as_deref() == Some(name)
            || resolve.id_of(*id).as_deref() == Some(name)
    });
    let id = matched
        .next()
        .ok_or_else(|| anyhow!("no interface named '{name}'"))?;
    // An interface in both roles is matched twice, so compare ids, not counts.
    if matched.any(|other| other != id) {
        bail!("'{name}' is ambiguous: qualify it as 'pkg:ns/name'");
    }
    Ok(id)
}

/// An item selected to be included in the world being built.
pub struct Selection<'s> {
    pub(crate) resolve: &'s Resolve,
    pub(crate) interfaces: Vec<InterfaceId>,
    pub(crate) functions: Vec<String>,
    /// The world that owns the selected functions. Absent when only interfaces
    /// were selected.
    pub(crate) world: Option<wit_parser::WorldId>,
}

impl<'s> Selection<'s> {
    fn empty(resolve: &'s Resolve) -> Self {
        Selection {
            resolve,
            interfaces: Vec::new(),
            functions: Vec::new(),
            world: None,
        }
    }

    fn of_interface(resolve: &'s Resolve, id: InterfaceId) -> Self {
        Selection {
            resolve,
            interfaces: vec![id],
            functions: Vec::new(),
            world: None,
        }
    }

    /// Whether nothing was selected.
    pub fn is_empty(&self) -> bool {
        self.interfaces.is_empty() && self.functions.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKAGE: &str = r"package test:src;
        interface greeter { greet: func(name: string) -> string; }
        interface logger { log: func(message: string); }
        world w {
          import logger;
          export greeter;
          export run: func();
        }";

    const EMPTY_WORLD: &str = "package test:empty;\n\nworld w {}\n";

    /// A component implementing an empty world.
    fn component_bytes() -> Vec<u8> {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("empty.wit", EMPTY_WORLD).expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");

        let mut core = wasm_encoder::Module::new().finish();
        wit_component::embed_component_metadata(
            &mut core,
            &resolve,
            world,
            wit_component::StringEncoding::UTF8,
        )
        .expect("embed");
        wit_component::ComponentEncoder::default()
            .module(&core)
            .expect("module")
            .encode()
            .expect("encode a component")
    }

    /// The same world as an encoded WIT package.
    fn package_bytes() -> Vec<u8> {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("empty.wit", EMPTY_WORLD).expect("parse");
        wit_component::encode(&resolve, package).expect("encode a package")
    }

    #[test]
    fn wit_text_is_read_as_a_package() {
        let source = WitSource::from_bytes(PACKAGE.as_bytes()).expect("decode");
        assert!(matches!(source, WitSource::Package(_)));
    }

    #[test]
    fn component_bytes_are_read_as_a_world() {
        let source = WitSource::from_bytes(&component_bytes()).expect("decode");
        assert!(matches!(source, WitSource::World(_)));
    }

    #[test]
    fn bytes_that_are_neither_wasm_nor_text_are_rejected() {
        let Err(error) = WitSource::from_bytes(&[0xFF, 0xFE]) else {
            panic!("invalid input must fail");
        };
        assert!(format!("{error:#}").contains("neither wasm"), "{error:#}");
    }

    #[test]
    fn a_package_is_parsed_from_wit_text() {
        assert!(PackageSource::from_text(PACKAGE).is_ok());
    }

    #[test]
    fn a_package_is_decoded_from_package_bytes() {
        assert!(PackageSource::from_bytes(&package_bytes()).is_ok());
    }

    #[test]
    fn a_package_rejects_component_bytes() {
        let Err(error) = PackageSource::from_bytes(&component_bytes()) else {
            panic!("a component is not a WIT package");
        };
        assert!(format!("{error:#}").contains("component"), "{error:#}");
    }

    #[test]
    fn a_world_is_decoded_from_component_bytes() {
        assert!(WorldSource::from_component(&component_bytes()).is_ok());
    }

    #[test]
    fn a_world_rejects_package_bytes() {
        let Err(error) = WorldSource::from_component(&package_bytes()) else {
            panic!("a WIT package is not a component");
        };
        assert!(format!("{error:#}").contains("WIT package"), "{error:#}");
    }

    #[test]
    fn a_package_narrows_to_a_named_world() {
        let package = PackageSource::from_text(PACKAGE).expect("parse");
        assert!(package.world("w").is_ok());
    }

    #[test]
    fn a_world_that_is_not_declared_is_rejected() {
        let package = PackageSource::from_text(PACKAGE).expect("parse");
        let Err(error) = package.world("absent") else {
            panic!("no such world");
        };
        assert!(format!("{error:#}").contains("absent"), "{error:#}");
    }

    #[test]
    fn a_package_selects_an_interface_by_name() {
        let package = PackageSource::from_text(PACKAGE).expect("parse");
        let selection = package.interface("greeter").expect("select");
        assert_eq!(selection.interfaces.len(), 1);
        assert!(selection.functions.is_empty());
    }

    #[test]
    fn an_interface_is_selected_by_its_qualified_name() {
        let package = PackageSource::from_text(PACKAGE).expect("parse");
        let selection = package.interface("test:src/greeter").expect("select");
        assert_eq!(selection.interfaces.len(), 1);
    }

    /// A resolve holding two packages that each declare `greeter`.
    fn two_packages() -> PackageSource {
        let mut resolve = Resolve::new();
        resolve
            .push_str(
                "dep.wit",
                "package dep:lib;
                 interface greeter { a: func(); }",
            )
            .expect("dep");
        let package = resolve
            .push_str(
                "main.wit",
                "package test:main;
                 interface greeter { b: func(); }
                 world w { import dep:lib/greeter; export greeter; }",
            )
            .expect("main");
        PackageSource { resolve, package }
    }

    #[test]
    fn a_short_name_matching_two_interfaces_is_ambiguous_in_a_package_source() {
        let Err(error) = two_packages().interface("greeter") else {
            panic!("two interfaces share this short name");
        };
        assert!(format!("{error:#}").contains("ambiguous"), "{error:#}");
    }

    #[test]
    fn a_qualified_name_picks_between_packages() {
        let packages = two_packages();
        assert!(packages.interface("dep:lib/greeter").is_ok());
        assert!(packages.interface("test:main/greeter").is_ok());
    }

    #[test]
    fn one_interface_in_both_roles_is_not_ambiguous() {
        let world = PackageSource::from_text(
            r"package test:dual;
              interface greeter { greet: func(); }
              world w { import greeter; export greeter; }",
        )
        .expect("parse")
        .world("w")
        .expect("world");
        assert!(world.interface("greeter").is_ok());
    }

    #[test]
    fn a_short_name_matching_two_interfaces_is_ambiguous_in_a_world_source() {
        let world = two_packages().world("w").expect("world");
        let Err(error) = world.interface("greeter") else {
            panic!("the imported and exported greeters are different interfaces");
        };
        assert!(format!("{error:#}").contains("ambiguous"), "{error:#}");
    }

    #[test]
    fn a_world_selects_by_role() {
        let world = PackageSource::from_text(PACKAGE)
            .expect("parse")
            .world("w")
            .expect("world");
        let exports = world.exports();
        // One interface and one world-level function.
        assert_eq!(exports.interfaces.len(), 1);
        assert_eq!(exports.functions, ["run"]);
        assert!(exports.world.is_some(), "a function is owned by a world");

        let imports = world.imports();
        assert_eq!(imports.interfaces.len(), 1);
        assert!(imports.functions.is_empty());
        assert!(
            imports.world.is_none(),
            "no function selected, so no owner world"
        );
    }

    #[test]
    fn a_world_selects_an_interface_from_either_role() {
        let world = PackageSource::from_text(PACKAGE)
            .expect("parse")
            .world("w")
            .expect("world");
        assert!(world.interface("greeter").is_ok(), "an export");
        assert!(world.interface("logger").is_ok(), "an import");
        assert!(world.interface("absent").is_err());
    }

    #[test]
    fn a_world_selects_a_world_level_function() {
        let world = PackageSource::from_text(PACKAGE)
            .expect("parse")
            .world("w")
            .expect("world");
        let selection = world.function("run").expect("select");
        assert_eq!(selection.functions, ["run"]);
        assert!(selection.interfaces.is_empty());

        let Err(error) = world.function("greet") else {
            panic!("'greet' is an interface function, not a world-level function");
        };
        assert!(format!("{error:#}").contains("greet"), "{error:#}");
    }

    #[test]
    fn an_empty_selection_reports_itself() {
        let package = PackageSource::from_text(PACKAGE).expect("parse");
        assert!(Selection::empty(&package.resolve).is_empty());
        assert!(!package.interface("greeter").expect("select").is_empty());
    }
}
