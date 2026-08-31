//! The Canonical ABI: mapping functions between a component and a core module.

use anyhow::{Result, bail};
use wasm_encoder::{ConstExpr, GlobalType, MemoryType, ValType};
use wit_parser::abi::{AbiVariant, WasmSignature, WasmType};
use wit_parser::{
    Function, Int, Mangling, Resolve, SizeAlign, TypeDefKind, TypeId, WorldId, WorldItem, WorldKey,
};

use crate::emitter::Emitter;
use crate::module::{
    CoreFunction, CoreGlobal, CoreImport, CoreMemory, CoreModule, Data, TypeTable, align8,
};

/// The heap pointer that allocations advance.
const HEAP: u32 = 0;

/// The name the component encoder expects for the allocator.
const REALLOC: &str = "cabi_realloc";

/// The name the component encoder expects for linear memory.
const MEMORY: &str = "memory";

/// One import in the core function index space, at its declaration position.
pub enum ImportEntry {
    /// A function the component imports.
    Func {
        /// The owning interface, or `None` for a world-level function.
        interface: Option<WorldKey>,
        func: String,
    },
    /// The `resource.drop` builtin for an imported resource type.
    Drop {
        /// Always an interface: a resource is owned by the one that defines it.
        interface: WorldKey,
        resource: TypeId,
        resource_name: String,
    },
}

/// One exported function to generate and how the ABI lowers it.
pub struct ExportEntry {
    /// The owning interface, or `None` for a world-level function.
    pub interface: Option<WorldKey>,
    pub func: String,
    /// The core param count, which is where a body's own locals begin.
    pub param_count: u32,
    /// Position among the async exports, to locate this function's
    /// `task.return` builtin.
    pub async_index: Option<usize>,
}

/// The imports in declaration order. First, per interface: functions then
/// resource drops; next, world-level functions then resource drops.
pub fn import_entries(resolve: &Resolve, world: WorldId) -> Vec<ImportEntry> {
    let world = &resolve.worlds[world];
    let mut entries = Vec::new();
    for (key, item) in world.imports.iter() {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let interface = &resolve.interfaces[*id];
        for name in interface.functions.keys() {
            entries.push(ImportEntry::Func {
                interface: Some(key.clone()),
                func: name.clone(),
            });
        }
        for (name, id) in interface.types.iter() {
            if matches!(resolve.types[*id].kind, TypeDefKind::Resource) {
                entries.push(ImportEntry::Drop {
                    interface: key.clone(),
                    resource: *id,
                    resource_name: name.clone(),
                });
            }
        }
    }
    for (key, item) in world.imports.iter() {
        match item {
            WorldItem::Function(func) => entries.push(ImportEntry::Func {
                interface: None,
                func: func.name.clone(),
            }),
            WorldItem::Type { id, .. } if is_resource(resolve, *id) => {
                // A `use`d resource's drop belongs to the interface that
                // defines it, not to the world-level `use` name.
                let interface = resolve
                    .type_interface_dep(*id)
                    .map(WorldKey::Interface)
                    .unwrap_or_else(|| key.clone());
                entries.push(ImportEntry::Drop {
                    interface,
                    resource: *id,
                    resource_name: resolve.types[*id].name.clone().unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
    entries
}

/// The exported functions in declaration order, and how the ABI lowers each.
pub fn export_entries(resolve: &Resolve, world: WorldId) -> Vec<ExportEntry> {
    let mut entries = Vec::new();
    let mut async_count = 0;
    for (key, func) in export_functions(resolve, world) {
        let variant = export_variant(func);
        let async_index = func.kind.is_async().then(|| {
            let index = async_count;
            async_count += 1;
            index
        });
        entries.push(ExportEntry {
            interface: key.cloned(),
            func: func.name.clone(),
            param_count: core_signature(resolve, func, variant).0.len() as u32,
            async_index,
        });
    }
    entries
}

/// The core index of an imported function.
pub fn import_index(
    entries: &[ImportEntry],
    interface: Option<&WorldKey>,
    func: &str,
) -> Option<u32> {
    entries
        .iter()
        .position(|entry| {
            matches!(entry, ImportEntry::Func { interface: owner, func: name }
                if owner.as_ref() == interface && name == func)
        })
        .map(|position| position as u32)
}

/// The core index of the allocator a body calls to reserve memory.
/// It is the first function the module defines, so it follows every import.
pub fn allocator_index(resolve: &Resolve, world: WorldId) -> u32 {
    let declared = import_entries(resolve, world).len();
    let builtins = export_entries(resolve, world)
        .iter()
        .filter(|entry| entry.async_index.is_some())
        .count();
    (declared + builtins) as u32
}

/// The core index of the `task.return` builtin for an async export. The
/// builtins follow the declared imports, with one per async export in
/// declaration order.
pub fn task_return_index(resolve: &Resolve, world: WorldId, async_index: usize) -> u32 {
    (import_entries(resolve, world).len() + async_index) as u32
}

/// The core index of an imported resource type's `resource.drop` builtin.
pub fn drop_index(entries: &[ImportEntry], resource: TypeId) -> Option<u32> {
    entries
        .iter()
        .position(
            |entry| matches!(entry, ImportEntry::Drop { resource: id, .. } if *id == resource),
        )
        .map(|position| position as u32)
}

/// The ABI variant for lifting an exported function. Async exports are
/// stackful, delivering through `task.return` and returning nothing.
pub fn export_variant(func: &wit_parser::Function) -> AbiVariant {
    if func.kind.is_async() {
        AbiVariant::GuestExportAsyncStackful
    } else {
        AbiVariant::GuestExport
    }
}

/// The ABI variant for lowering a call to an imported function.
pub fn import_variant(func: &wit_parser::Function) -> AbiVariant {
    if func.kind.is_async() {
        AbiVariant::GuestImportAsync
    } else {
        AbiVariant::GuestImport
    }
}

/// How a component's types are laid out in linear memory.
/// Sizes and offsets are wasm32 byte counts.
pub struct Layout {
    sizes: SizeAlign,
}

impl Layout {
    /// Built once per resolve, since it walks every type in the arena.
    pub fn new(resolve: &Resolve) -> Self {
        let mut sizes = SizeAlign::default();
        sizes.fill(resolve);
        Layout { sizes }
    }

    /// The bytes `ty` occupies.
    pub fn size(&self, ty: &wit_parser::Type) -> usize {
        self.sizes.size(ty).size_wasm32()
    }

    /// The byte offset of each type in a record laid out in order.
    pub fn field_offsets<'a>(
        &self,
        types: impl IntoIterator<Item = &'a wit_parser::Type>,
    ) -> Vec<usize> {
        self.sizes
            .field_offsets(types)
            .into_iter()
            .map(|(offset, _)| offset.size_wasm32())
            .collect()
    }

    /// The bytes a record of `types` occupies, including trailing padding,
    /// thus also the stride between consecutive elements.
    pub fn record_size<'a>(&self, types: impl IntoIterator<Item = &'a wit_parser::Type>) -> usize {
        self.sizes.record(types).size.size_wasm32()
    }

    /// The byte offset of a variant's payload, after its discriminant and any
    /// padding the payload's alignment requires.
    pub fn payload_offset<'a>(
        &self,
        tag: Int,
        cases: impl IntoIterator<Item = Option<&'a wit_parser::Type>>,
    ) -> usize {
        self.sizes.payload_offset(tag, cases).size_wasm32()
    }
}

/// A type's core representation, in canonical flattening order.
pub fn flat_types(resolve: &Resolve, ty: wit_parser::Type) -> Result<Vec<ValType>> {
    let mut buffer = [WasmType::I32; 64];
    let mut flat = wit_parser::abi::FlatTypes::new(&mut buffer);
    if !resolve.push_flat(&ty, &mut flat) {
        bail!("type {ty:?} flattens to more than 64 core values");
    }
    Ok(flat.to_vec().iter().map(|&ty| val_type(ty)).collect())
}

/// A function's core signature under `variant`.
pub fn core_signature(
    resolve: &Resolve,
    func: &Function,
    variant: AbiVariant,
) -> (Vec<ValType>, Vec<ValType>) {
    val_types(&resolve.wasm_signature(variant, func))
}

/// Whether an exported function writes its result into memory.
pub fn export_returns_indirectly(resolve: &Resolve, func: &Function) -> bool {
    resolve.wasm_signature(export_variant(func), func).retptr
}

/// Whether an imported function writes its result into memory.
pub fn import_returns_indirectly(resolve: &Resolve, func: &Function) -> bool {
    resolve.wasm_signature(import_variant(func), func).retptr
}

/// An exported function's core signature, plus the params of the cleanup
/// function it needs.
///
/// Cleanup is needed only when the result is returned through memory rather
/// than in core values: the export returns a pointer, and the caller passes it
/// back once it has read the value, so the memory can be released. Async
/// exports deliver through `task.return`, so nothing is left to release.
pub fn export_signature(
    resolve: &Resolve,
    func: &Function,
) -> (Vec<ValType>, Vec<ValType>, Option<Vec<ValType>>) {
    let signature = resolve.wasm_signature(export_variant(func), func);
    let (params, results) = val_types(&signature);
    let cleanup = (!func.kind.is_async() && signature.retptr).then(|| results.clone());
    (params, results, cleanup)
}

/// The name a function is exported under, with async exports prefixed to mark
/// them as stackful so the component encoder is able to classify the lift.
/// Uses "legacy" mangling, pending async support in the standard.
pub fn export_name(resolve: &Resolve, interface: Option<&WorldKey>, func: &Function) -> String {
    let interface = interface.map(|key| resolve.name_world_key(key));
    let name = func
        .legacy_core_export_name(interface.as_deref())
        .into_owned();
    if func.kind.is_async() {
        format!("[async-lift-stackful]{name}")
    } else {
        name
    }
}

/// The module name an import is declared under. World-level functions share
/// the root module name.
pub fn import_module_name(resolve: &Resolve, interface: Option<&WorldKey>) -> String {
    match interface {
        Some(key) => resolve.name_world_key(key),
        None => "$root".to_string(),
    }
}

/// A signature's params and results as core types.
fn val_types(signature: &WasmSignature) -> (Vec<ValType>, Vec<ValType>) {
    (
        signature.params.iter().copied().map(val_type).collect(),
        signature.results.iter().copied().map(val_type).collect(),
    )
}

/// A flattened type as a core type. Pointers and lengths are 32-bit on wasm32;
/// `PointerOrI64` is a 64-bit slot and narrowing it would truncate.
fn val_type(ty: WasmType) -> ValType {
    match ty {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
        WasmType::F32 => ValType::F32,
        WasmType::F64 => ValType::F64,
    }
}

/// Each interface export's functions paired with the interface, then each
/// world-level export function.
fn export_functions(
    resolve: &Resolve,
    world: WorldId,
) -> impl Iterator<Item = (Option<&WorldKey>, &wit_parser::Function)> {
    resolve.worlds[world]
        .exports
        .iter()
        .flat_map(move |(key, item)| match item {
            WorldItem::Interface { id, .. } => resolve.interfaces[*id]
                .functions
                .values()
                .map(move |func| (Some(key), func))
                .collect::<Vec<_>>(),
            WorldItem::Function(func) => vec![(None, func)],
            WorldItem::Type { .. } => Vec::new(),
        })
}

/// Whether `id` is a resource, following `use` aliases to the definition.
fn is_resource(resolve: &Resolve, id: TypeId) -> bool {
    match &resolve.types[id].kind {
        TypeDefKind::Resource => true,
        TypeDefKind::Type(wit_parser::Type::Id(inner)) => is_resource(resolve, *inner),
        _ => false,
    }
}

/// One generated export: which function it implements, and its finished body.
pub struct GeneratedFunction {
    pub interface: Option<WorldKey>,
    pub func: String,
    pub body: wasm_encoder::Function,
}

/// Assemble the core module a component of this world lifts from.
///
/// The function index space is laid out here:
/// 1. the component's declared imports
/// 2. one `task.return` builtin per async export
/// 3. the allocator
/// 4. the generated bodies
/// 5. a cleanup function for each export that needs one
pub fn core_module(
    resolve: &Resolve,
    world: WorldId,
    generated: Vec<GeneratedFunction>,
    types: TypeTable,
    data: Data,
) -> Result<CoreModule> {
    let mut imports = Vec::new();
    for entry in import_entries(resolve, world) {
        let module = import_module_name(resolve, entry_interface(&entry));
        imports.push(match entry {
            ImportEntry::Func { interface, func } => {
                let declared = imported_function(resolve, world, interface.as_ref(), &func)?;
                let (params, results) = core_signature(resolve, declared, AbiVariant::GuestImport);
                CoreImport {
                    module,
                    name: func,
                    params,
                    results,
                }
            }
            ImportEntry::Drop { resource_name, .. } => CoreImport {
                module,
                name: format!("[resource-drop]{resource_name}"),
                params: vec![ValType::I32],
                results: Vec::new(),
            },
        });
    }
    for (interface, func) in export_functions(resolve, world) {
        if !func.kind.is_async() {
            continue;
        }
        let (module, name, signature) =
            func.task_return_import(resolve, interface, Mangling::Legacy);
        let (params, results) = val_types(&signature);
        imports.push(CoreImport {
            module,
            name,
            params,
            results,
        });
    }

    debug_assert_eq!(
        imports.len() as u32,
        allocator_index(resolve, world),
        "the allocator follows every import"
    );
    let heap_base = align8(data.len()) as i32;
    let mut functions = vec![CoreFunction {
        params: vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
        results: vec![ValType::I32],
        body: allocator_body(),
        export_name: REALLOC.to_string(),
    }];

    let mut cleanups = Vec::new();
    for generated in generated {
        let declared = exported_function(
            resolve,
            world,
            generated.interface.as_ref(),
            &generated.func,
        )?;
        let (params, results, cleanup) = export_signature(resolve, declared);
        let name = export_name(resolve, generated.interface.as_ref(), declared);
        if let Some(cleanup_params) = cleanup {
            cleanups.push(CoreFunction {
                params: cleanup_params,
                results: Vec::new(),
                body: heap_reset_body(heap_base),
                export_name: format!("cabi_post_{name}"),
            });
        }
        functions.push(CoreFunction {
            params,
            results,
            body: generated.body,
            export_name: name,
        });
    }
    functions.extend(cleanups);

    Ok(CoreModule {
        imports,
        functions,
        memories: vec![CoreMemory {
            ty: MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            },
            export_name: Some(MEMORY.to_string()),
        }],
        globals: vec![CoreGlobal {
            ty: GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            init: ConstExpr::i32_const(heap_base),
        }],
        types,
        data,
    })
}

/// A bump allocator: hand out the next aligned address, then advance past it.
/// Called with the canonical ABI's `(old_ptr, old_len, align, new_len)`.
///
/// A resize request traps, because the allocator cannot grow a block in place,
/// and returning fresh memory while abandoning the old contents would silently
/// lose data.
///
/// `align` is ignored because every address handed out is 8-aligned, which
/// satisfies any alignment a wasm32 type can require.
///
/// Aligning on entry allows callers to request a type's own size: a size that
/// is not a multiple of 8 leaves the heap global holding an unaligned address,
/// but the next call rounds it up before returning a pointer, so no store ever
/// sees a misaligned base.
fn allocator_body() -> wasm_encoder::Function {
    let emitter = Emitter::new(4);
    let pointer = emitter.local(ValType::I32);
    // This allocator cannot support a resize request.
    emitter.emit(wasm_encoder::Instruction::LocalGet(0));
    emitter.emit(wasm_encoder::Instruction::LocalGet(1));
    emitter.emit(wasm_encoder::Instruction::I32Or);
    emitter
        .if_(wasm_encoder::BlockType::Empty, || {
            emitter.trap();
            Ok(())
        })
        .expect("no frames to close");
    // The next 8-aligned address, which is what this call returns.
    emitter.emit(wasm_encoder::Instruction::GlobalGet(HEAP));
    emitter.emit(wasm_encoder::Instruction::I32Const(7));
    emitter.emit(wasm_encoder::Instruction::I32Add);
    emitter.emit(wasm_encoder::Instruction::I32Const(-8));
    emitter.emit(wasm_encoder::Instruction::I32And);
    emitter.emit(wasm_encoder::Instruction::LocalSet(pointer));
    // Advance past the request.
    emitter.emit(wasm_encoder::Instruction::LocalGet(pointer));
    emitter.emit(wasm_encoder::Instruction::LocalGet(3));
    emitter.emit(wasm_encoder::Instruction::I32Add);
    emitter.emit(wasm_encoder::Instruction::GlobalSet(HEAP));
    emitter.emit(wasm_encoder::Instruction::LocalGet(pointer));
    emitter.encode().expect("no frames to close")
}

/// Releases everything allocated during a call by resetting the heap.
fn heap_reset_body(heap_base: i32) -> wasm_encoder::Function {
    let emitter = Emitter::new(0);
    emitter.emit(wasm_encoder::Instruction::I32Const(heap_base));
    emitter.emit(wasm_encoder::Instruction::GlobalSet(HEAP));
    emitter.encode().expect("no frames to close")
}

/// The interface an entry belongs to, if any.
fn entry_interface(entry: &ImportEntry) -> Option<&WorldKey> {
    match entry {
        ImportEntry::Func { interface, .. } => interface.as_ref(),
        ImportEntry::Drop { interface, .. } => Some(interface),
    }
}

/// The declared function behind an import entry.
fn imported_function<'a>(
    resolve: &'a Resolve,
    world: WorldId,
    interface: Option<&WorldKey>,
    func: &str,
) -> Result<&'a Function> {
    let world = &resolve.worlds[world];
    let found = match interface {
        Some(key) => match world.imports.get(key) {
            Some(WorldItem::Interface { id, .. }) => resolve.interfaces[*id].functions.get(func),
            _ => None,
        },
        None => world.imports.values().find_map(|item| match item {
            WorldItem::Function(f) if f.name == func => Some(f),
            _ => None,
        }),
    };
    found.ok_or_else(|| anyhow::anyhow!("import '{func}' is not declared by this world"))
}

/// The declared function behind a generated export.
pub fn exported_function<'a>(
    resolve: &'a Resolve,
    world: WorldId,
    interface: Option<&WorldKey>,
    func: &str,
) -> Result<&'a Function> {
    export_functions(resolve, world)
        .find(|(key, declared)| *key == interface && declared.name == func)
        .map(|(_, declared)| declared)
        .ok_or_else(|| anyhow::anyhow!("export '{func}' is not declared by this world"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Core opcodes, for the assertions that read an emitted body.
    const UNREACHABLE: u8 = 0x00;
    const IF: u8 = 0x04;
    const END: u8 = 0x0B;
    const LOCAL_GET: u8 = 0x20;
    const LOCAL_SET: u8 = 0x21;
    const GLOBAL_GET: u8 = 0x23;
    const I32_CONST: u8 = 0x41;
    const I32_ADD: u8 = 0x6A;
    const I32_AND: u8 = 0x71;
    const I32_OR: u8 = 0x72;

    /// Parse `wit` and return its single world.
    fn world(wit: &str) -> (Resolve, WorldId) {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("test.wit", wit).expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");
        (resolve, world)
    }

    fn import_names(entries: &[ImportEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                ImportEntry::Func { func, .. } => func.clone(),
                ImportEntry::Drop { resource_name, .. } => format!("[drop]{resource_name}"),
            })
            .collect()
    }

    /// The only function of the only world, with its interface key.
    fn only_function(resolve: &Resolve, world: WorldId) -> (Option<WorldKey>, Function) {
        let entry = &export_entries(resolve, world)[0];
        let func = export_functions(resolve, world)
            .find(|(_, f)| f.name == entry.func)
            .expect("the export")
            .1
            .clone();
        (entry.interface.clone(), func)
    }

    /// The named type in the sole interface of `wit`.
    fn named_type(resolve: &Resolve, w: WorldId, name: &str) -> wit_parser::Type {
        let id = resolve.worlds[w]
            .imports
            .values()
            .find_map(|item| match item {
                WorldItem::Interface { id, .. } => resolve.interfaces[*id].types.get(name).copied(),
                _ => None,
            })
            .expect("the type");
        wit_parser::Type::Id(id)
    }

    /// A body that returns nothing.
    fn empty_body() -> wasm_encoder::Function {
        Emitter::new(0).encode().expect("empty")
    }

    fn validate(module: CoreModule) -> Vec<u8> {
        let bytes = module.encode();
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("the assembled module must be valid wasm");
        bytes
    }

    #[test]
    fn imports_are_ordered_by_interface_then_world_level() {
        let (resolve, world) = world(
            r"package test:order;
              interface first { a: func(); b: func(); }
              interface second { c: func(); }
              world w {
                import first;
                import second;
                import x: func();
              }",
        );
        let entries = import_entries(&resolve, world);
        assert_eq!(import_names(&entries), vec!["a", "b", "c", "x"]);
    }

    #[test]
    fn a_resource_drop_follows_its_function() {
        let (resolve, world) = world(
            r"package test:drops;
              interface store { resource handle; open: func(); }
              world w { import store; }",
        );
        let entries = import_entries(&resolve, world);
        assert_eq!(import_names(&entries), vec!["open", "[drop]handle"]);
    }

    #[test]
    fn same_named_functions_in_different_interfaces_get_their_own_index() {
        let (resolve, world) = world(
            r"package test:collide;
              interface first { get: func(); }
              interface second { get: func(); }
              world w { import first; import second; }",
        );
        let entries = import_entries(&resolve, world);
        let key = |name: &str| {
            resolve.worlds[world]
                .imports
                .keys()
                .find(|k| resolve.name_world_key(k).ends_with(name))
                .cloned()
                .expect("interface in world")
        };
        assert_eq!(
            import_index(&entries, Some(&key("first")), "get"),
            Some(0),
            "the first interface's `get`"
        );
        assert_eq!(
            import_index(&entries, Some(&key("second")), "get"),
            Some(1),
            "the second interface's `get`"
        );
    }

    #[test]
    fn a_world_level_function_is_found_without_an_interface() {
        let (resolve, world) = world(
            r"package test:bare;
              interface iface { run: func(); }
              world w { import iface; import run: func(); }",
        );
        let entries = import_entries(&resolve, world);
        assert_eq!(import_index(&entries, None, "run"), Some(1));
    }

    #[test]
    fn a_resource_drop_is_found_by_its_type() {
        let (resolve, world) = world(
            r"package test:drop;
              interface store { resource handle; }
              world w { import store; }",
        );
        let entries = import_entries(&resolve, world);
        let resource = match &entries[0] {
            ImportEntry::Drop { resource, .. } => *resource,
            _ => panic!("expected a drop entry"),
        };
        assert_eq!(drop_index(&entries, resource), Some(0));
    }

    #[test]
    fn exports_carry_their_core_param_count() {
        let (resolve, world) = world(
            r"package test:params;
              world w {
                export nothing: func();
                export two: func(a: s32, b: s32);
                export text: func(s: string);
              }",
        );
        let entries = export_entries(&resolve, world);
        let counts: Vec<(String, u32)> = entries
            .iter()
            .map(|e| (e.func.clone(), e.param_count))
            .collect();
        assert_eq!(
            counts,
            vec![
                ("nothing".to_string(), 0),
                ("two".to_string(), 2),
                // a string lowers to a pointer and a length
                ("text".to_string(), 2),
            ]
        );
    }

    #[test]
    fn async_exports_are_numbered_in_declaration_order() {
        let (resolve, world) = world(
            r"package test:ordering;
              world w {
                export sync-one: func();
                export async-one: async func();
                export sync-two: func();
                export async-two: async func();
              }",
        );
        let entries = export_entries(&resolve, world);
        let indices: Vec<(String, Option<usize>)> = entries
            .iter()
            .map(|e| (e.func.clone(), e.async_index))
            .collect();
        assert_eq!(
            indices,
            vec![
                ("sync-one".to_string(), None),
                ("async-one".to_string(), Some(0)),
                ("sync-two".to_string(), None),
                ("async-two".to_string(), Some(1)),
            ]
        );
    }

    #[test]
    fn async_exports_are_lifted_stackful() {
        let (resolve, world) = world(
            r"package test:lifting;
              world w { export a: func(); export b: async func(); }",
        );
        let exports: Vec<AbiVariant> = export_functions(&resolve, world)
            .map(|(_, func)| export_variant(func))
            .collect();
        assert_eq!(
            exports,
            vec![
                AbiVariant::GuestExport,
                AbiVariant::GuestExportAsyncStackful
            ]
        );
    }

    #[test]
    fn primitives_flatten_to_one_core_value_each() {
        let (resolve, _) = world(r"package test:flat; world w { export f: func(); }");
        assert_eq!(
            flat_types(&resolve, wit_parser::Type::U8).unwrap(),
            vec![ValType::I32]
        );
        assert_eq!(
            flat_types(&resolve, wit_parser::Type::S64).unwrap(),
            vec![ValType::I64]
        );
        assert_eq!(
            flat_types(&resolve, wit_parser::Type::F64).unwrap(),
            vec![ValType::F64]
        );
    }

    #[test]
    fn a_string_flattens_to_a_pointer_and_a_length() {
        let (resolve, _) = world(r"package test:text; world w { export f: func(); }");
        assert_eq!(
            flat_types(&resolve, wit_parser::Type::String).unwrap(),
            vec![ValType::I32, ValType::I32]
        );
    }

    #[test]
    fn a_joined_variant_payload_keeps_its_widest_slot() {
        let (resolve, w) = world(
            r"package test:joined;
              interface iface {
                variant v { text(string), wide(u64) }
                f: func(x: v);
              }
              world w { import iface; }",
        );
        let variant = resolve.worlds[w]
            .imports
            .values()
            .find_map(|item| match item {
                WorldItem::Interface { id, .. } => resolve.interfaces[*id]
                    .types
                    .values()
                    .copied()
                    .find(|id| matches!(resolve.types[*id].kind, TypeDefKind::Variant(_))),
                _ => None,
            })
            .expect("the variant");
        // A discriminant, then the joined payload where the string's pointer
        // and the u64 share one 64-bit slot, then the string's length.
        assert_eq!(
            flat_types(&resolve, wit_parser::Type::Id(variant)).unwrap(),
            vec![ValType::I32, ValType::I64, ValType::I32]
        );
    }

    #[test]
    fn a_direct_result_needs_no_cleanup() {
        let (resolve, w) = world(r"package test:direct; world w { export f: func() -> s32; }");
        let (_, func) = only_function(&resolve, w);
        let (params, results, cleanup) = export_signature(&resolve, &func);
        assert!(params.is_empty());
        assert_eq!(results, vec![ValType::I32]);
        assert_eq!(cleanup, None);
    }

    #[test]
    fn an_indirect_result_is_cleaned_up_by_its_own_results() {
        let (resolve, w) = world(r"package test:indirect; world w { export f: func() -> string; }");
        let (_, func) = only_function(&resolve, w);
        let (_, results, cleanup) = export_signature(&resolve, &func);
        // A string is returned via memory, so the result is a pointer to it.
        assert_eq!(results, vec![ValType::I32]);
        assert_eq!(cleanup, Some(results));
    }

    #[test]
    fn an_async_export_needs_no_cleanup() {
        let (resolve, w) =
            world(r"package test:slow; world w { export f: async func() -> string; }");
        let (_, func) = only_function(&resolve, w);
        let (.., cleanup) = export_signature(&resolve, &func);
        assert_eq!(cleanup, None);
    }

    #[test]
    fn an_async_export_name_is_marked_stackful() {
        let (resolve, w) = world(r"package test:naming; world w { export f: async func(); }");
        let (interface, func) = only_function(&resolve, w);
        assert_eq!(
            export_name(&resolve, interface.as_ref(), &func),
            "[async-lift-stackful]f"
        );
    }

    #[test]
    fn an_interface_export_name_is_qualified() {
        let (resolve, w) = world(
            r"package test:qualified;
              interface iface { run: func(); }
              world w { export iface; }",
        );
        let (interface, func) = only_function(&resolve, w);
        let name = export_name(&resolve, interface.as_ref(), &func);
        assert!(name.ends_with("iface#run"), "got {name}");
    }

    #[test]
    fn a_world_level_import_uses_the_root_module_name() {
        let (resolve, w) = world(
            r"package test:root;
              interface iface { run: func(); }
              world w { import iface; import solo: func(); }",
        );
        let entries = import_entries(&resolve, w);
        let interfaces: Vec<Option<WorldKey>> = entries
            .iter()
            .map(|e| match e {
                ImportEntry::Func { interface, .. } => interface.clone(),
                ImportEntry::Drop { interface, .. } => Some(interface.clone()),
            })
            .collect();
        assert_eq!(
            import_module_name(&resolve, interfaces[1].as_ref()),
            "$root"
        );
        assert!(
            import_module_name(&resolve, interfaces[0].as_ref()).contains("iface"),
            "an interface import carries its qualified name"
        );
    }

    #[test]
    fn primitive_sizes_are_their_byte_widths() {
        let (resolve, _) = world(r"package test:sizes; world w { export f: func(); }");
        let layout = Layout::new(&resolve);
        assert_eq!(layout.size(&wit_parser::Type::U8), 1);
        assert_eq!(layout.size(&wit_parser::Type::U16), 2);
        assert_eq!(layout.size(&wit_parser::Type::U32), 4);
        assert_eq!(layout.size(&wit_parser::Type::U64), 8);
        // A string is a pointer and a length.
        assert_eq!(layout.size(&wit_parser::Type::String), 8);
    }

    #[test]
    fn record_fields_are_offset_by_their_alignment() {
        let (resolve, w) = world(
            r"package test:offsets;
              interface iface { record r { a: u8, b: u64, c: u8 } f: func(x: r); }
              world w { import iface; }",
        );
        let layout = Layout::new(&resolve);
        let fields = [
            wit_parser::Type::U8,
            wit_parser::Type::U64,
            wit_parser::Type::U8,
        ];
        // The u64 forces 8-alignment, so it starts at 8 rather than 1.
        assert_eq!(layout.field_offsets(fields.iter()), vec![0, 8, 16]);
        // And the record is padded out to a multiple of its 8-byte alignment.
        assert_eq!(layout.record_size(fields.iter()), 24);
        assert_eq!(layout.size(&named_type(&resolve, w, "r")), 24);
    }

    #[test]
    fn a_variant_payload_follows_its_discriminant() {
        let (resolve, w) = world(
            r"package test:payload;
              interface iface {
                variant v { small(u8), wide(u64) }
                f: func(x: v);
              }
              world w { import iface; }",
        );
        let layout = Layout::new(&resolve);
        let cases = [Some(&wit_parser::Type::U8), Some(&wit_parser::Type::U64)];
        // A 1-byte discriminant, then padding to the widest case's alignment.
        assert_eq!(layout.payload_offset(Int::U8, cases), 8);
        assert_eq!(layout.size(&named_type(&resolve, w, "v")), 16);
    }

    #[test]
    fn an_assembled_module_is_valid() {
        let (resolve, w) = world(r"package test:assemble; world w { export run: func(); }");
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "run".to_string(),
            body: empty_body(),
        }];
        let module = core_module(
            &resolve,
            w,
            generated,
            TypeTable::default(),
            Data::default(),
        )
        .unwrap();
        validate(module);
    }

    #[test]
    fn a_body_for_an_undeclared_export_is_reported() {
        // Assembly resolves every body against the world's declared exports.
        let (resolve, w) = world(r"package test:missing; world w { export run: func(); }");
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "absent".to_string(),
            body: empty_body(),
        }];
        let Err(error) = core_module(
            &resolve,
            w,
            generated,
            TypeTable::default(),
            Data::default(),
        ) else {
            panic!("an undeclared export must fail");
        };
        assert!(
            format!("{error:#}").contains("absent"),
            "the error names the function: {error:#}"
        );
    }

    #[test]
    fn the_allocator_is_exported_with_expected_signature() {
        let (resolve, w) = world(r"package test:alloc; world w { export run: func(); }");
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "run".to_string(),
            body: empty_body(),
        }];
        let module = core_module(
            &resolve,
            w,
            generated,
            TypeTable::default(),
            Data::default(),
        )
        .unwrap();
        // The allocator is the first defined function, and takes four params.
        assert_eq!(module.functions[0].export_name, "cabi_realloc");
        assert_eq!(module.functions[0].params.len(), 4);
        validate(module);
    }

    /// The allocator body from a minimal assembled module.
    fn allocator_bytes(package: &str) -> Vec<u8> {
        let (resolve, w) = world(&format!(
            "package test:{package}; world w {{ export run: func(); }}"
        ));
        let module = core_module(
            &resolve,
            w,
            vec![GeneratedFunction {
                interface: None,
                func: "run".to_string(),
                body: empty_body(),
            }],
            TypeTable::default(),
            Data::default(),
        )
        .unwrap();
        module.functions[0].body.clone().into_raw_body()
    }

    #[test]
    fn the_allocator_traps_on_a_resize_it_cannot_honor() {
        // Returning fresh memory and abandoning the old contents would
        // silently lose data, so a non-zero `old_ptr` or `old_len` traps.
        let body = allocator_bytes("allocresize");
        let guard = body
            .windows(4)
            .position(|window| window == [LOCAL_GET, 0x00, LOCAL_GET, 0x01])
            .expect("both old_ptr and old_len are read");
        assert_eq!(body[guard + 4], I32_OR, "they are combined: {body:02x?}");
        assert_eq!(body[guard + 5], IF, "tested for nonzero: {body:02x?}");
        assert_eq!(body[guard + 7], UNREACHABLE, "the arm traps: {body:02x?}");
    }

    #[test]
    fn the_allocator_ignores_the_requested_alignment() {
        // Every address handed out is 8-aligned, which satisfies any alignment
        // a wasm32 type can require, so the param is never read.
        let body = allocator_bytes("allocalignparam");
        assert!(
            !body.windows(2).any(|window| window == [LOCAL_GET, 0x02]),
            "local 2 is never read: {body:02x?}"
        );
    }

    #[test]
    fn the_allocator_returns_the_rounded_address() {
        let body = allocator_bytes("allocalign");
        // The rounding computed from the heap (`0x78` is -8 in signed LEB128).
        let rounding = [
            GLOBAL_GET, HEAP as u8, I32_CONST, 7, I32_ADD, I32_CONST, 0x78, I32_AND,
        ];
        let at = body
            .windows(rounding.len())
            .position(|window| window == rounding)
            .expect("the heap rounded up to the next multiple of 8");
        let stored = at + rounding.len();
        assert_eq!(
            body[stored], LOCAL_SET,
            "the rounded address is stored: {body:02x?}"
        );
        let rounded = body[stored + 1];
        assert_eq!(
            &body[body.len() - 3..],
            &[LOCAL_GET, rounded, END],
            "the function returns the stored local: {body:02x?}"
        );
    }

    #[test]
    fn imports_precede_the_allocator_in_the_index_space() {
        let (resolve, w) = world(
            r"package test:indexed;
              interface logger { log: func(); }
              world w { import logger; export run: func(); }",
        );
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "run".to_string(),
            body: empty_body(),
        }];
        let module = core_module(
            &resolve,
            w,
            generated,
            TypeTable::default(),
            Data::default(),
        )
        .unwrap();
        assert_eq!(module.imports.len(), 1);
        // One import. So allocator is index 1 and generated body index 2.
        assert_eq!(module.functions.len(), 2);
        validate(module);
    }

    #[test]
    fn an_indirect_result_gets_a_cleanup_export() {
        let (resolve, w) =
            world(r"package test:cleanup; world w { export run: func() -> string; }");
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "run".to_string(),
            body: {
                // The signature returns a pointer, so the body must leave one.
                let emitter = Emitter::new(0);
                emitter.emit(wasm_encoder::Instruction::I32Const(0));
                emitter.encode().expect("body")
            },
        }];
        let module = core_module(
            &resolve,
            w,
            generated,
            TypeTable::default(),
            Data::default(),
        )
        .unwrap();
        let names: Vec<&str> = module
            .functions
            .iter()
            .map(|f| f.export_name.as_str())
            .collect();
        assert_eq!(names, vec!["cabi_realloc", "run", "cabi_post_run"]);
        validate(module);
    }

    #[test]
    fn a_direct_result_gets_no_cleanup_export() {
        let (resolve, w) = world(r"package test:nocleanup; world w { export run: func() -> s32; }");
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "run".to_string(),
            body: {
                let emitter = Emitter::new(0);
                emitter.emit(wasm_encoder::Instruction::I32Const(0));
                emitter.encode().expect("body")
            },
        }];
        let module = core_module(
            &resolve,
            w,
            generated,
            TypeTable::default(),
            Data::default(),
        )
        .unwrap();
        let names: Vec<&str> = module
            .functions
            .iter()
            .map(|f| f.export_name.as_str())
            .collect();
        assert_eq!(names, vec!["cabi_realloc", "run"]);
    }

    #[test]
    fn an_assembled_module_declares_a_heap_global() {
        let (resolve, w) = world(r"package test:heap; world w { export run: func(); }");
        let mut data = Data::default();
        data.intern(b"hello");
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "run".to_string(),
            body: empty_body(),
        }];
        let module = core_module(&resolve, w, generated, TypeTable::default(), data).unwrap();
        assert_eq!(module.globals.len(), 1);
        validate(module);
    }

    #[test]
    fn an_assembled_module_componentizes() {
        let (resolve, w) = world(r"package test:whole; world w { export run: func(); }");
        let generated = vec![GeneratedFunction {
            interface: None,
            func: "run".to_string(),
            body: empty_body(),
        }];
        let module = core_module(
            &resolve,
            w,
            generated,
            TypeTable::default(),
            Data::default(),
        )
        .unwrap();
        let mut core = module.encode();
        wit_component::embed_component_metadata(
            &mut core,
            &resolve,
            w,
            wit_component::StringEncoding::UTF8,
        )
        .expect("metadata");
        let component = wit_component::ComponentEncoder::default()
            .module(&core)
            .expect("module")
            .validate(true)
            .encode()
            .expect("the core module must satisfy the component encoder");
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&component)
            .expect("the component must be valid");
    }
}
