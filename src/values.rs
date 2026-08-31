//! A layer between the World and the ABI for reading and writing values.
//!
//! [`crate::abi`] states the rules for flat types, layouts, and signatures.
//! This layer carries them out for a particular value at a particular place:
//! [`Slot`] says where it sits, [`Writer`] fills it from a [`ValueSpec`], and
//! [`Loader`] lifts it onto the stack.
//!
//! The navigable `Value` a factory holds in its World is one layer above this.
//! It crosses down to here as a [`ValueRef`], which is a type and a slot
//! detached from the navigable World model.

use std::cell::RefCell;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail};
use wasm_encoder::{BlockType, Instruction, MemArg, ValType};
use wit_parser::{Int, Resolve, TypeDefKind, WorldId};

use crate::abi::{self, ImportEntry, Layout};
use crate::emitter::Emitter;
use crate::module::{Data, TypeTable};

/// Everything a function body reads or appends to, for the whole component.
pub struct BuildContext {
    resolve: Rc<Resolve>,
    world: WorldId,
    layout: Layout,
    /// The imports in core index order.
    imports: Vec<ImportEntry>,
    /// The allocator's core index, called whenever a value needs memory.
    allocator: u32,
    module_state: RefCell<ModuleState>,
}

/// What the bodies append to as they emit, and the module encodes at the end.
#[derive(Default)]
struct ModuleState {
    data: Data,
    types: TypeTable,
}

impl BuildContext {
    pub fn new(resolve: Rc<Resolve>, world: WorldId) -> Self {
        let layout = Layout::new(&resolve);
        let imports = abi::import_entries(&resolve, world);
        let allocator = abi::allocator_index(&resolve, world);
        BuildContext {
            resolve,
            world,
            layout,
            imports,
            allocator,
            module_state: RefCell::new(ModuleState::default()),
        }
    }

    pub(crate) fn resolve(&self) -> &Resolve {
        &self.resolve
    }

    pub(crate) fn world(&self) -> WorldId {
        self.world
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    pub(crate) fn imports(&self) -> &[ImportEntry] {
        &self.imports
    }

    pub(crate) fn allocator(&self) -> u32 {
        self.allocator
    }

    // Each interner borrows, acts, and releases; the cell is never handed out,
    // and neither takes a callback, so the borrow cannot be re-entered.

    /// Intern bytes into the data segment, returning where they landed.
    pub(crate) fn intern(&self, bytes: &[u8]) -> (u32, u32) {
        self.module_state.borrow_mut().data.intern(bytes)
    }

    /// Intern a func type, returning its index in the type section.
    pub(crate) fn func_type(&self, params: &[ValType], results: &[ValType]) -> u32 {
        self.module_state
            .borrow_mut()
            .types
            .func_type(params, results)
    }

    /// Take what the bodies appended, once they have all finished.
    pub(crate) fn take_module_state(&self) -> (TypeTable, Data) {
        let state = std::mem::take(&mut *self.module_state.borrow_mut());
        (state.types, state.data)
    }
}

/// A wasm local: its index, and the core type it was declared as. The pair is
/// inseparable: a local's declared type is what a value must be bitcast to
/// before it can be stored there.
#[derive(Clone, Copy)]
pub struct Local {
    pub index: u32,
    pub ty: ValType,
}

impl Local {
    pub fn new(index: u32, ty: ValType) -> Self {
        Local { index, ty }
    }
}

/// Where a value lives: in locals, or in memory.
///
/// `Flat` means the locals themselves hold it, in canonical flat order; a
/// composite member occupies a contiguous sub-range of them.
///
/// `Memory` means it is at `base + offset`, where `base` is a local holding a
/// pointer.
///
/// Both bottom out in locals. They differ in whether the local is the value
/// itself or points at the value's memory location.
#[derive(Clone)]
pub enum Slot {
    Memory { base: u32, offset: usize },
    Flat { locals: Vec<Local> },
}

impl Slot {
    /// A memory slot at the start of the area `base` points at.
    pub fn at(base: u32) -> Self {
        Slot::Memory { base, offset: 0 }
    }

    pub fn flat(locals: Vec<Local>) -> Self {
        Slot::Flat { locals }
    }

    /// The base-pointer local, for a value in memory.
    pub fn base(&self) -> Option<u32> {
        match self {
            Slot::Memory { base, .. } => Some(*base),
            Slot::Flat { .. } => None,
        }
    }

    /// The byte offset from `base`, or zero for a flat value.
    pub fn offset(&self) -> usize {
        match self {
            Slot::Memory { offset, .. } => *offset,
            Slot::Flat { .. } => 0,
        }
    }

    /// The locals holding a flat value, empty for one in memory.
    pub fn locals(&self) -> &[Local] {
        match self {
            Slot::Flat { locals } => locals,
            Slot::Memory { .. } => &[],
        }
    }

    /// The slot of a member: `bytes` further into the memory form, or the
    /// `[from, to)` sub-range of the locals. Both coordinates are supplied
    /// because only the caller knows the byte offset and the flat span.
    pub fn member(&self, bytes: usize, from: usize, to: usize) -> Slot {
        match self {
            Slot::Memory { base, offset } => Slot::Memory {
                base: *base,
                offset: offset + bytes,
            },
            Slot::Flat { locals } => Slot::Flat {
                locals: locals[from..to].to_vec(),
            },
        }
    }
}

/// A sequence's element count, and where it comes from.
///
/// Building from a recipe, the count is a number held while emitting; producing
/// from a visitor, only the running component knows it.
#[derive(Clone, Copy)]
pub enum Len {
    Const(usize),
    In(Local),
}

/// A raw-typed value: what it is, and where it lives.
///
/// The lower-layer pairing. This layer cannot name the navigable `Value`, so a
/// materialized value crosses down as this.
#[derive(Clone)]
pub struct ValueRef {
    pub ty: wit_parser::Type,
    pub slot: Slot,
}

/// One leaf of a spec: a literal, or content something else supplies.
///
/// One variant per WIT primitive, each carrying the exact Rust type, so a
/// value that cannot fit its WIT type cannot be constructed.
pub enum Leaf {
    Str(String),
    /// A `list<u8>` literal, interned like a string.
    Bytes(Vec<u8>),
    Bool(bool),
    S8(i8),
    S16(i16),
    S32(i32),
    S64(i64),
    U8(u8),
    U16(u16),
    U32(u32),
    U64(u64),
    F32(f32),
    F64(f64),
    Char(char),
    /// Parts joined into one `string` or `list<u8>`, allocated and copied at
    /// runtime.
    Concat(Vec<ValueSpec>),
    /// Content an already-materialized value supplies, emitted as a copy.
    Source(ValueRef),
}

/// What to write into a value: a recipe, not a value itself.
pub enum ValueSpec {
    Record(Vec<(String, ValueSpec)>),
    Option(Option<Box<ValueSpec>>),
    Ok(Box<ValueSpec>),
    Err(Box<ValueSpec>),
    Variant {
        case: String,
        payload: Option<Box<ValueSpec>>,
    },
    List(Vec<ValueSpec>),
    Tuple(Vec<ValueSpec>),
    Flags(Vec<String>),
    Map(Vec<(ValueSpec, ValueSpec)>),
    Leaf(Leaf),
}

impl ValueSpec {
    pub fn string(s: impl Into<String>) -> ValueSpec {
        ValueSpec::Leaf(Leaf::Str(s.into()))
    }

    /// A `string` when present, `none` otherwise.
    pub fn optional_string(s: Option<impl Into<String>>) -> ValueSpec {
        match s {
            Some(s) => ValueSpec::some(ValueSpec::string(s)),
            None => ValueSpec::none(),
        }
    }

    pub fn bytes(bytes: impl Into<Vec<u8>>) -> ValueSpec {
        ValueSpec::Leaf(Leaf::Bytes(bytes.into()))
    }

    pub fn bool(b: bool) -> ValueSpec {
        ValueSpec::Leaf(Leaf::Bool(b))
    }

    pub fn s8(n: i8) -> ValueSpec {
        ValueSpec::Leaf(Leaf::S8(n))
    }

    pub fn s16(n: i16) -> ValueSpec {
        ValueSpec::Leaf(Leaf::S16(n))
    }

    pub fn s32(n: i32) -> ValueSpec {
        ValueSpec::Leaf(Leaf::S32(n))
    }

    pub fn s64(n: i64) -> ValueSpec {
        ValueSpec::Leaf(Leaf::S64(n))
    }

    pub fn u8(n: u8) -> ValueSpec {
        ValueSpec::Leaf(Leaf::U8(n))
    }

    pub fn u16(n: u16) -> ValueSpec {
        ValueSpec::Leaf(Leaf::U16(n))
    }

    pub fn u32(n: u32) -> ValueSpec {
        ValueSpec::Leaf(Leaf::U32(n))
    }

    pub fn u64(n: u64) -> ValueSpec {
        ValueSpec::Leaf(Leaf::U64(n))
    }

    pub fn f32(n: f32) -> ValueSpec {
        ValueSpec::Leaf(Leaf::F32(n))
    }

    pub fn f64(n: f64) -> ValueSpec {
        ValueSpec::Leaf(Leaf::F64(n))
    }

    pub fn char(c: char) -> ValueSpec {
        ValueSpec::Leaf(Leaf::Char(c))
    }

    pub fn none() -> ValueSpec {
        ValueSpec::Option(None)
    }

    pub fn some(value: impl Into<ValueSpec>) -> ValueSpec {
        ValueSpec::Option(Some(Box::new(value.into())))
    }

    pub fn ok(value: impl Into<ValueSpec>) -> ValueSpec {
        ValueSpec::Ok(Box::new(value.into()))
    }

    pub fn err(value: impl Into<ValueSpec>) -> ValueSpec {
        ValueSpec::Err(Box::new(value.into()))
    }

    pub fn record(
        fields: impl IntoIterator<Item = (impl Into<String>, impl Into<ValueSpec>)>,
    ) -> ValueSpec {
        ValueSpec::Record(
            fields
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        )
    }

    /// A variant case carrying a payload.
    pub fn variant(case: impl Into<String>, payload: impl Into<ValueSpec>) -> ValueSpec {
        ValueSpec::Variant {
            case: case.into(),
            payload: Some(Box::new(payload.into())),
        }
    }

    /// A variant case without a payload, which is how an enum is represented.
    pub fn variant_unit(case: impl Into<String>) -> ValueSpec {
        ValueSpec::Variant {
            case: case.into(),
            payload: None,
        }
    }

    pub fn list(items: impl IntoIterator<Item = impl Into<ValueSpec>>) -> ValueSpec {
        let items: Vec<ValueSpec> = items.into_iter().map(Into::into).collect();
        // A list of byte literals is a `list<u8>` that can be interned, so it
        // takes the same path a `bytes` literal does.
        let bytes: Option<Vec<u8>> = items
            .iter()
            .map(|item| match item {
                ValueSpec::Leaf(Leaf::U8(byte)) => Some(*byte),
                _ => None,
            })
            .collect();
        if let Some(bytes) = bytes.filter(|bytes| !bytes.is_empty()) {
            return ValueSpec::Leaf(Leaf::Bytes(bytes));
        }
        ValueSpec::List(items)
    }

    pub fn tuple(members: impl IntoIterator<Item = impl Into<ValueSpec>>) -> ValueSpec {
        ValueSpec::Tuple(members.into_iter().map(Into::into).collect())
    }

    pub fn flags(names: impl IntoIterator<Item = impl Into<String>>) -> ValueSpec {
        ValueSpec::Flags(names.into_iter().map(Into::into).collect())
    }

    pub fn map(entries: impl IntoIterator<Item = (ValueSpec, ValueSpec)>) -> ValueSpec {
        ValueSpec::Map(entries.into_iter().collect())
    }

    /// Content an already-materialized value supplies.
    pub fn source(source: ValueRef) -> ValueSpec {
        ValueSpec::Leaf(Leaf::Source(source))
    }

    /// Parts joined into one `string` or `list<u8>`, based on the destination
    /// type. Adjacent literals are joined here, so if all parts are literals,
    /// they become a single interned entry.
    pub fn concat(parts: impl IntoIterator<Item = impl Into<ValueSpec>>) -> ValueSpec {
        let mut joined: Vec<ValueSpec> = Vec::new();
        for part in parts {
            let parts = match part.into() {
                ValueSpec::Leaf(Leaf::Concat(parts)) => parts,
                spec => vec![spec],
            };
            for part in parts {
                match (joined.last_mut(), part) {
                    (Some(ValueSpec::Leaf(Leaf::Str(text))), ValueSpec::Leaf(Leaf::Str(next))) => {
                        text.push_str(&next)
                    }
                    (
                        Some(ValueSpec::Leaf(Leaf::Bytes(bytes))),
                        ValueSpec::Leaf(Leaf::Bytes(next)),
                    ) => bytes.extend_from_slice(&next),
                    (_, part) => joined.push(part),
                }
            }
        }
        if joined.len() == 1
            && matches!(
                joined[0],
                ValueSpec::Leaf(Leaf::Str(_)) | ValueSpec::Leaf(Leaf::Bytes(_))
            )
        {
            return joined.pop().expect("one part");
        }
        ValueSpec::Leaf(Leaf::Concat(joined))
    }
}

/// Lifts a value out of linear memory onto the stack, in canonical flat order.
pub struct Loader<'a> {
    ctx: &'a BuildContext,
    emitter: &'a Emitter,
}

impl<'a> Loader<'a> {
    pub fn new(ctx: &'a BuildContext, emitter: &'a Emitter) -> Self {
        Loader { ctx, emitter }
    }

    fn emit(&self, instruction: Instruction<'static>) {
        self.emitter.emit(instruction);
    }

    /// Leave `ty`'s flats on the stack, read from `[base]`, and report their
    /// core types.
    pub fn load(&self, ty: wit_parser::Type, base: u32) -> Result<Vec<ValType>> {
        let flats = abi::flat_types(self.ctx.resolve(), ty)?;
        self.load_at(ty, base, 0, &flats)?;
        Ok(flats)
    }

    /// `expected` is the core signature these flats must have. It equals
    /// `ty`'s own flats at the top level, but inside a variant arm it is the
    /// joined slot types, so a narrow case widens into them. Its length always
    /// covers `ty`'s flats, which is what keeps the slicing below in bounds.
    fn load_at(
        &self,
        ty: wit_parser::Type,
        base: u32,
        offset: usize,
        expected: &[ValType],
    ) -> Result<()> {
        let id = match ty {
            wit_parser::Type::Bool | wit_parser::Type::U8 | wit_parser::Type::S8 => {
                return self.leaf(base, offset, Load::I32From8, expected);
            }
            wit_parser::Type::U16 | wit_parser::Type::S16 => {
                return self.leaf(base, offset, Load::I32From16, expected);
            }
            wit_parser::Type::U32
            | wit_parser::Type::S32
            | wit_parser::Type::Char
            | wit_parser::Type::ErrorContext => {
                return self.leaf(base, offset, Load::I32, expected);
            }
            wit_parser::Type::U64 | wit_parser::Type::S64 => {
                return self.leaf(base, offset, Load::I64, expected);
            }
            wit_parser::Type::F32 => return self.leaf(base, offset, Load::F32, expected),
            wit_parser::Type::F64 => return self.leaf(base, offset, Load::F64, expected),
            wit_parser::Type::String => return self.pointer_and_length(base, offset, expected),
            wit_parser::Type::Id(id) => id,
        };
        match self.ctx.resolve().types[id].kind.clone() {
            TypeDefKind::Type(inner) => self.load_at(inner, base, offset, expected),
            // A handle is one i32, whatever it refers to.
            TypeDefKind::Handle(_) | TypeDefKind::Future(_) | TypeDefKind::Stream(_) => {
                self.leaf(base, offset, Load::I32, expected)
            }
            TypeDefKind::Record(record) => {
                let types: Vec<wit_parser::Type> = record.fields.iter().map(|f| f.ty).collect();
                self.load_members(&types, base, offset, expected)
            }
            TypeDefKind::Tuple(tuple) => self.load_members(&tuple.types, base, offset, expected),
            TypeDefKind::List(_) | TypeDefKind::Map(_, _) => {
                self.pointer_and_length(base, offset, expected)
            }
            TypeDefKind::FixedLengthList(elem, count) => {
                let types = vec![elem; count as usize];
                self.load_members(&types, base, offset, expected)
            }
            TypeDefKind::Enum(e) => self.leaf(base, offset, Load::for_tag(e.tag()), expected),
            TypeDefKind::Flags(flags) => {
                // One i32 word per 32 flags.
                for word in 0..expected.len() {
                    self.leaf(
                        base,
                        offset + word * 4,
                        Load::I32,
                        &expected[word..word + 1],
                    )?;
                }
                let _ = flags;
                Ok(())
            }
            TypeDefKind::Option(inner) => {
                self.load_variant(&[None, Some(inner)], Int::U8, base, offset, expected)
            }
            TypeDefKind::Result(result) => {
                self.load_variant(&[result.ok, result.err], Int::U8, base, offset, expected)
            }
            TypeDefKind::Variant(variant) => {
                let cases: Vec<Option<wit_parser::Type>> =
                    variant.cases.iter().map(|case| case.ty).collect();
                self.load_variant(&cases, variant.tag(), base, offset, expected)
            }
            TypeDefKind::Resource => bail!("a resource type has no value representation"),
            TypeDefKind::Unknown => unreachable!("unresolved type"),
        }
    }

    /// A variant-like value: its discriminant, then the joined payload flats.
    ///
    /// One `if`/`else` per case, each arm leaving the full joined payload width
    /// on the stack so every branch agrees with the block's result type.
    fn load_variant(
        &self,
        cases: &[Option<wit_parser::Type>],
        tag: Int,
        base: u32,
        offset: usize,
        expected: &[ValType],
    ) -> Result<()> {
        self.leaf(base, offset, Load::for_tag(tag), &expected[0..1])?;
        let payload_expected = &expected[1..];
        if payload_expected.is_empty() {
            // Every case is a unit case: the discriminant is the whole value.
            return Ok(());
        }
        let payload_offset = offset
            + self
                .ctx
                .layout()
                .payload_offset(tag, cases.iter().map(|case| case.as_ref()));
        let block = BlockType::FunctionType(self.ctx.func_type(&[], payload_expected));
        self.load_arms(
            cases,
            tag,
            base,
            offset,
            payload_offset,
            0,
            payload_expected,
            block,
        )
    }

    /// The `if`/`else` chain, one level per case. The discriminant is reloaded
    /// for each test rather than kept on the stack, which the arm's own result
    /// occupies.
    #[allow(clippy::too_many_arguments)]
    fn load_arms(
        &self,
        cases: &[Option<wit_parser::Type>],
        tag: Int,
        base: u32,
        disc_offset: usize,
        payload_offset: usize,
        case: usize,
        expected: &[ValType],
        block: BlockType,
    ) -> Result<()> {
        if case + 1 == cases.len() {
            // The last case needs no test. Every other case has been ruled out.
            return self.load_payload(&cases[case], base, payload_offset, expected);
        }
        self.emit(Instruction::LocalGet(base));
        self.emit(Load::for_tag(tag).instruction(disc_offset));
        self.emit(Instruction::I32Const(case as i32));
        self.emit(Instruction::I32Eq);
        self.emit(Instruction::If(block));
        self.load_payload(&cases[case], base, payload_offset, expected)?;
        self.emit(Instruction::Else);
        self.load_arms(
            cases,
            tag,
            base,
            disc_offset,
            payload_offset,
            case + 1,
            expected,
            block,
        )?;
        self.emit(Instruction::End);
        Ok(())
    }

    /// One case's payload, padded out to the joined width.
    ///
    /// A case narrower than the join pushes zeros for the slots it does not
    /// fill, since every arm must leave the same stack shape.
    fn load_payload(
        &self,
        case: &Option<wit_parser::Type>,
        base: u32,
        offset: usize,
        expected: &[ValType],
    ) -> Result<()> {
        let filled = match case {
            Some(ty) => {
                let width = abi::flat_types(self.ctx.resolve(), *ty)?.len();
                self.load_at(*ty, base, offset, &expected[..width])?;
                width
            }
            None => 0,
        };
        for ty in &expected[filled..] {
            self.emit(zero(*ty));
        }
        Ok(())
    }

    /// Each member's flats at its own offset, in order.
    fn load_members(
        &self,
        types: &[wit_parser::Type],
        base: u32,
        offset: usize,
        expected: &[ValType],
    ) -> Result<()> {
        let offsets = self.ctx.layout().field_offsets(types.iter());
        let mut remaining = expected;
        for (ty, member_offset) in types.iter().zip(offsets) {
            let width = abi::flat_types(self.ctx.resolve(), *ty)?.len();
            let (mine, rest) = remaining.split_at(width);
            self.load_at(*ty, base, offset + member_offset, mine)?;
            remaining = rest;
        }
        Ok(())
    }

    /// The `{pointer, length}` pair that represents a string, list, or map.
    fn pointer_and_length(&self, base: u32, offset: usize, expected: &[ValType]) -> Result<()> {
        self.leaf(base, offset, Load::I32, &expected[0..1])?;
        self.leaf(base, offset + 4, Load::I32, &expected[1..2])
    }

    /// One leaf load, reconciled with its destination slot.
    fn leaf(&self, base: u32, offset: usize, load: Load, expected: &[ValType]) -> Result<()> {
        self.emit(Instruction::LocalGet(base));
        self.emit(load.instruction(offset));
        if let Some(&want) = expected.first() {
            for instruction in bitcast(load.result(), want)? {
                self.emit(instruction);
            }
        }
        Ok(())
    }
}

/// Lowers a spec into the slot a value occupies.
///
/// The counterpart of [`Loader`] (which lifts bytes onto the stack); this
/// fills a slot from a [`ValueSpec`].
pub struct Writer<'a> {
    ctx: &'a BuildContext,
    emitter: &'a Emitter,
}

impl<'a> Writer<'a> {
    pub fn new(ctx: &'a BuildContext, emitter: &'a Emitter) -> Self {
        Writer { ctx, emitter }
    }

    fn emit(&self, instruction: Instruction<'static>) {
        self.emitter.emit(instruction);
    }

    /// Fill `slot` with `value`, a spec for something of type `ty`.
    pub fn write(&self, ty: wit_parser::Type, slot: &Slot, value: &ValueSpec) -> Result<()> {
        // A source supplies content this layer did not author, so it
        // short-circuits the type-directed walk.
        if let ValueSpec::Leaf(Leaf::Source(source)) = value {
            return match slot {
                Slot::Flat { locals } => self.copy_flat_from(source, locals),
                _ => self.copy_from(source, slot),
            };
        }
        // A non-composite in a flat slot is the locals themselves, so there is
        // nothing to walk. A composite falls through to the walk below, which
        // descends into sub-ranges of those locals.
        if let Slot::Flat { locals } = slot
            && !matches!(ty, wit_parser::Type::Id(_))
        {
            return self.write_flat(ty, locals, value);
        }
        match ty {
            wit_parser::Type::String
            | wit_parser::Type::Bool
            | wit_parser::Type::S8
            | wit_parser::Type::S16
            | wit_parser::Type::S32
            | wit_parser::Type::S64
            | wit_parser::Type::U8
            | wit_parser::Type::U16
            | wit_parser::Type::U32
            | wit_parser::Type::U64
            | wit_parser::Type::F32
            | wit_parser::Type::F64
            | wit_parser::Type::Char => {
                let (base, offset) = self.memory_dest(slot)?;
                self.write_memory(ty, base, offset, value)
            }
            wit_parser::Type::Id(id) => self.write_defined(id, slot, value),
            other => bail!("unsupported type {other:?}"),
        }
    }

    /// The `(base, offset)` of a memory slot, or error if not a memory slot.
    fn memory_dest(&self, slot: &Slot) -> Result<(u32, usize)> {
        let base = slot
            .base()
            .ok_or_else(|| anyhow!("can only build into a memory slot"))?;
        Ok((base, slot.offset()))
    }

    /// Write a leaf into locals, reconciling each against its declared type.
    fn write_flat(&self, ty: wit_parser::Type, locals: &[Local], value: &ValueSpec) -> Result<()> {
        let ValueSpec::Leaf(leaf) = value else {
            bail!(
                "a composite destination must be in memory (only a value that \
                lives directly in locals can be written flat)"
            );
        };
        match leaf {
            Leaf::Str(text) => {
                if !matches!(ty, wit_parser::Type::String) {
                    bail!("a string value cannot be written to a {ty:?} position");
                }
                self.write_interned_flat(text.as_bytes(), locals)
            }
            Leaf::Bytes(bytes) => {
                if !is_byte_sequence(self.ctx.resolve(), ty) {
                    bail!("a list<u8> value cannot be written to a {ty:?} position");
                }
                self.write_interned_flat(bytes, locals)
            }
            Leaf::Concat(parts) => self.write_concat(ty, &Slot::flat(locals.to_vec()), parts),
            Leaf::Source(source) => self.copy_flat_from(source, locals),
            scalar => {
                // The slot may be wider than this value: a variant's payload
                // locals are the join of every case, so a 1-flat case fills
                // only the first and the rest remain zero.
                let Some(&local) = locals.first() else {
                    bail!("a scalar needs at least one local, got 0");
                };
                let (push, actual) = push_scalar(scalar, ty)?;
                self.emit(push);
                self.set_local(local, actual)
            }
        }
    }

    /// Write a leaf into memory at `base + offset`.
    fn write_memory(
        &self,
        ty: wit_parser::Type,
        base: u32,
        offset: usize,
        value: &ValueSpec,
    ) -> Result<()> {
        let ValueSpec::Leaf(leaf) = value else {
            bail!("expected a leaf value for a primitive/string node");
        };
        match leaf {
            Leaf::Str(text) => {
                if !matches!(ty, wit_parser::Type::String) {
                    bail!("a string value cannot be written to a {ty:?} position");
                }
                self.write_interned_memory(text.as_bytes(), base, offset)
            }
            Leaf::Bytes(bytes) => {
                if !is_byte_sequence(self.ctx.resolve(), ty) {
                    bail!("a list<u8> value cannot be written to a {ty:?} position");
                }
                self.write_interned_memory(bytes, base, offset)
            }
            Leaf::Concat(parts) => self.write_concat(ty, &Slot::Memory { base, offset }, parts),
            Leaf::Source(_) => unreachable!("a source is handled in write() before type dispatch"),
            scalar => {
                let (push, stored) = push_scalar(scalar, ty)?;
                self.emit(Instruction::LocalGet(base));
                self.emit(push);
                self.emit(Store::for_type(stored)?.instruction(offset));
                Ok(())
            }
        }
    }

    /// Write a value of a defined type: anything a `Type::Id` can name.
    /// A `Local` carries its own declared type, so slicing a slot to a member
    /// brings the types with it and each arm bitcasts to what it finds.
    fn write_defined(&self, id: wit_parser::TypeId, slot: &Slot, value: &ValueSpec) -> Result<()> {
        match &self.ctx.resolve().types[id].kind {
            TypeDefKind::Type(inner) => self.write(*inner, slot, value),
            // A resource, future, or stream handle is a single i32; the
            // supplied value is the handle, so it stores like any scalar.
            TypeDefKind::Handle(_) | TypeDefKind::Future(_) | TypeDefKind::Stream(_) => {
                if let Slot::Flat { locals } = slot {
                    return self.write_flat(wit_parser::Type::Id(id), locals, value);
                }
                let (base, offset) = self.memory_dest(slot)?;
                self.write_memory(wit_parser::Type::Id(id), base, offset, value)
            }
            TypeDefKind::Record(record) => {
                let ValueSpec::Record(supplied) = value else {
                    bail!("expected a Record value for a record type");
                };
                let types: Vec<wit_parser::Type> =
                    record.fields.iter().map(|field| field.ty).collect();
                let slots = member_slots(self.ctx, slot, &types)?;
                for (field, field_slot) in record.fields.iter().zip(slots) {
                    let field_value = supplied
                        .iter()
                        .find(|(name, _)| name == &field.name)
                        .map(|(_, value)| value)
                        .ok_or_else(|| {
                            anyhow!("no value supplied for record field '{}'", field.name)
                        })?;
                    self.write(field.ty, &field_slot, field_value)
                        .with_context(|| format!("in field '{}'", field.name))?;
                }
                if let Some((unknown, _)) = supplied
                    .iter()
                    .find(|(name, _)| !record.fields.iter().any(|field| &field.name == name))
                {
                    bail!(
                        "no field '{unknown}' in this record (declared: {})",
                        record
                            .fields
                            .iter()
                            .map(|field| field.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                Ok(())
            }
            TypeDefKind::Option(inner) => {
                let inner = *inner;
                let ValueSpec::Option(supplied) = value else {
                    bail!("expected an Option value for an option<T> type");
                };
                let payload_offset = self.ctx.layout().payload_offset(Int::U8, [Some(&inner)]);
                match supplied {
                    None => {
                        self.write_disc(slot, Int::U8, 0, payload_offset)?;
                        Ok(())
                    }
                    Some(payload) => {
                        let payload_slot = self.write_disc(slot, Int::U8, 1, payload_offset)?;
                        self.write(inner, &payload_slot, payload)
                            .context("in some(..)")
                    }
                }
            }
            TypeDefKind::Result(result) => {
                let (ok, err) = (result.ok, result.err);
                let payload_offset = self
                    .ctx
                    .layout()
                    .payload_offset(Int::U8, [ok.as_ref(), err.as_ref()]);
                match value {
                    ValueSpec::Ok(payload) => {
                        let payload_slot = self.write_disc(slot, Int::U8, 0, payload_offset)?;
                        match ok {
                            Some(ty) => self.write(ty, &payload_slot, payload).context("in ok(..)"),
                            None => bail!("ok(..) value but result has no ok type"),
                        }
                    }
                    ValueSpec::Err(payload) => {
                        let payload_slot = self.write_disc(slot, Int::U8, 1, payload_offset)?;
                        match err {
                            Some(ty) => {
                                self.write(ty, &payload_slot, payload).context("in err(..)")
                            }
                            None => bail!("err(..) value but result has no err type"),
                        }
                    }
                    _ => bail!("expected an Ok/Err value for a result<T,E> type"),
                }
            }
            TypeDefKind::List(elem) => {
                let elem = *elem;
                // A `list<u8>` literal is interned as a unit (like a string)
                // rather than iterating its elements.
                if matches!(value, ValueSpec::Leaf(Leaf::Bytes(_) | Leaf::Concat(_))) {
                    return match slot {
                        Slot::Flat { locals } => {
                            self.write_flat(wit_parser::Type::Id(id), locals, value)
                        }
                        _ => {
                            let (base, offset) = self.memory_dest(slot)?;
                            self.write_memory(wit_parser::Type::Id(id), base, offset, value)
                        }
                    };
                }
                let ValueSpec::List(items) = value else {
                    bail!("expected a List value for a list<T> type");
                };
                let stride = self.ctx.layout().size(&elem);
                let pointer = self.allocate(items.len() * stride);
                // The elements live in the fresh allocation, not at an offset
                // from this node's base, which is what a slot expresses; so no
                // sub-writer is needed.
                for (index, item) in items.iter().enumerate() {
                    self.write(
                        elem,
                        &Slot::Memory {
                            base: pointer.index,
                            offset: index * stride,
                        },
                        item,
                    )
                    .with_context(|| format!("in element [{index}]"))?;
                }
                self.write_ptr_len(slot, pointer, Len::Const(items.len()))
            }
            TypeDefKind::Variant(variant) => {
                let ValueSpec::Variant { case, payload } = value else {
                    bail!("expected a Variant value for a variant type");
                };
                let (disc, case_ty) = variant
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, declared)| &declared.name == case)
                    .map(|(index, declared)| (index, declared.ty))
                    .ok_or_else(|| anyhow!("no variant case '{case}'"))?;
                let tag = variant.tag();
                let payload_offset = self.ctx.layout().payload_offset(
                    tag,
                    variant.cases.iter().map(|declared| declared.ty.as_ref()),
                );
                let payload_slot = self.write_disc(slot, tag, disc as i64, payload_offset)?;
                match (case_ty, payload) {
                    (Some(ty), Some(supplied)) => self
                        .write(ty, &payload_slot, supplied)
                        .with_context(|| format!("in case '{case}'")),
                    (None, None) => Ok(()),
                    (Some(_), None) => bail!("variant case '{case}' needs a payload"),
                    (None, Some(_)) => bail!("variant case '{case}' takes no payload"),
                }
            }
            TypeDefKind::Tuple(tuple) => {
                let ValueSpec::Tuple(members) = value else {
                    bail!("expected a Tuple value for a tuple type");
                };
                if members.len() != tuple.types.len() {
                    bail!(
                        "tuple expects {} members, got {}",
                        tuple.types.len(),
                        members.len()
                    );
                }
                let slots = member_slots(self.ctx, slot, &tuple.types)?;
                for (index, ((ty, member_slot), supplied)) in
                    tuple.types.iter().zip(slots).zip(members).enumerate()
                {
                    self.write(*ty, &member_slot, supplied)
                        .with_context(|| format!("in tuple member {index}"))?;
                }
                Ok(())
            }
            TypeDefKind::Enum(declared) => {
                // Every case is a unit case, so an enum takes the same spec as
                // a variant rather than a spelling of its own.
                let ValueSpec::Variant { case, payload } = value else {
                    bail!("expected a variant case for an enum type");
                };
                if payload.is_some() {
                    bail!("enum case '{case}' takes no payload");
                }
                let disc = declared
                    .cases
                    .iter()
                    .position(|declared| &declared.name == case)
                    .ok_or_else(|| anyhow!("no enum case '{case}'"))?;
                // No payloads, so the discriminant is the whole value.
                self.write_disc(slot, declared.tag(), disc as i64, 0)?;
                Ok(())
            }
            TypeDefKind::Flags(declared) => {
                let ValueSpec::Flags(set) = value else {
                    bail!("expected a Flags value for a flags type");
                };
                let count = declared.flags.len();
                let words = flag_words(declared, set)?;
                // Flat: the words are the value, one per repr word. Each is
                // i32-natural and may land in a wider joined slot, so it
                // reconciles like any other flat write.
                if let Slot::Flat { locals } = slot {
                    if words.len() > locals.len() {
                        bail!("flags need {} local(s), got {}", words.len(), locals.len());
                    }
                    for (local, bits) in locals.iter().zip(&words) {
                        self.set_local_const(*local, *bits as i32)?;
                    }
                    return Ok(());
                }
                let (base, offset) = self.memory_dest(slot)?;
                // No flags means no bytes: storing anything here would write
                // past a zero-width value into whatever follows it.
                let Some(&first) = words.first() else {
                    return Ok(());
                };
                if count <= 8 {
                    self.store_disc(base, offset, Int::U8, first as i64)
                } else if count <= 16 {
                    self.store_disc(base, offset, Int::U16, first as i64)
                } else {
                    for (index, bits) in words.iter().enumerate() {
                        self.store_const_i32(base, offset + index * 4, *bits as i32)?;
                    }
                    Ok(())
                }
            }
            TypeDefKind::Map(key, value_ty) => {
                let (key, value_ty) = (*key, *value_ty);
                let ValueSpec::Map(entries) = value else {
                    bail!("expected a Map value for a map<K,V> type");
                };
                // Entries are laid out as 2-member records, contiguous like a
                // list's elements.
                let pair = [key, value_ty];
                let stride = self.ctx.layout().record_size(pair.iter());
                let offsets = self.ctx.layout().field_offsets(pair.iter());
                let pointer = self.allocate(entries.len() * stride);
                for (index, (key_value, value_value)) in entries.iter().enumerate() {
                    self.write(
                        key,
                        &Slot::Memory {
                            base: pointer.index,
                            offset: index * stride + offsets[0],
                        },
                        key_value,
                    )
                    .with_context(|| format!("in entry [{index}] key"))?;
                    self.write(
                        value_ty,
                        &Slot::Memory {
                            base: pointer.index,
                            offset: index * stride + offsets[1],
                        },
                        value_value,
                    )
                    .with_context(|| format!("in entry [{index}] value"))?;
                }
                self.write_ptr_len(slot, pointer, Len::Const(entries.len()))
            }
            TypeDefKind::FixedLengthList(elem, count) => {
                let (elem, count) = (*elem, *count as usize);
                let ValueSpec::List(items) = value else {
                    bail!("expected a List value for a list<T, N> type");
                };
                if items.len() != count {
                    bail!(
                        "list<T, {count}> expects {count} elements, got {}",
                        items.len()
                    );
                }
                // N elements inline. No pointer/length, so they are written
                // directly into this slot. Positionally identical to a tuple
                // of N copies of the element type.
                let repeated = vec![elem; count];
                let slots = member_slots(self.ctx, slot, &repeated)?;
                for (index, (elem_slot, item)) in slots.into_iter().zip(items).enumerate() {
                    self.write(elem, &elem_slot, item)
                        .with_context(|| format!("in element [{index}]"))?;
                }
                Ok(())
            }
            other => bail!("unsupported type kind {other:?}"),
        }
    }

    /// Join string parts into one allocation and write its `{ptr, len}` pair.
    ///
    /// A source part's length is only known at runtime, so the total is summed
    /// into a local and each part is copied at an incrementing offset.
    fn write_concat(&self, ty: wit_parser::Type, slot: &Slot, parts: &[ValueSpec]) -> Result<()> {
        if !is_byte_sequence(self.ctx.resolve(), ty) {
            bail!(
                "a concat value can only be written to a string or list<u8> position, not {ty:?}"
            );
        }
        // A string destination must contain valid UTF-8, which constrains its
        // parts. A `list<u8>` destination accepts any byte sequence.
        let text_only = is_string(self.ctx.resolve(), ty);
        for part in parts {
            self.check_part(part, text_only)?;
        }
        let length = Local::new(self.emitter.local(ValType::I32), ValType::I32);
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(length.index));
        // Each part's `{ptr, len}`, resolved once so the copy loop below reads
        // the same locals the sum did.
        let mut resolved = Vec::with_capacity(parts.len());
        for part in parts {
            let (pointer, part_len) = self.part_ptr_len(part)?;
            self.emit(Instruction::LocalGet(length.index));
            self.emit(Instruction::LocalGet(part_len));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::LocalSet(length.index));
            resolved.push((pointer, part_len));
        }

        let pointer = Local::new(self.emitter.local(ValType::I32), ValType::I32);
        call_allocator(
            self.ctx,
            self.emitter,
            Size::Strided {
                count: length,
                stride: 1,
            },
        );
        self.emit(Instruction::LocalSet(pointer.index));

        let cursor = self.emitter.local(ValType::I32);
        self.emit(Instruction::LocalGet(pointer.index));
        self.emit(Instruction::LocalSet(cursor));
        for (part_ptr, part_len) in resolved {
            self.emit(Instruction::LocalGet(cursor));
            self.emit(Instruction::LocalGet(part_ptr));
            self.emit(Instruction::LocalGet(part_len));
            self.emit(Instruction::MemoryCopy {
                src_mem: 0,
                dst_mem: 0,
            });
            self.emit(Instruction::LocalGet(cursor));
            self.emit(Instruction::LocalGet(part_len));
            self.emit(Instruction::I32Add);
            self.emit(Instruction::LocalSet(cursor));
        }
        self.write_ptr_len(slot, pointer, Len::In(length))
    }

    /// Intern `bytes` and set a `{ptr, len}` pair of locals with its location.
    fn write_interned_flat(&self, bytes: &[u8], locals: &[Local]) -> Result<()> {
        let [pointer, length, ..] = locals[..] else {
            bail!(
                "a string/list<u8> needs two locals (ptr, len), got {}",
                locals.len()
            );
        };
        let (offset, len) = self.ctx.intern(bytes);
        self.set_local_const(pointer, offset as i32)?;
        self.set_local_const(length, len as i32)
    }

    /// Intern `bytes` and store the `{ptr, len}` pair at `base + offset`.
    fn write_interned_memory(&self, bytes: &[u8], base: u32, offset: usize) -> Result<()> {
        let (interned, len) = self.ctx.intern(bytes);
        self.store_const_i32(base, offset, interned as i32)?;
        self.store_const_i32(base, offset + 4, len as i32)
    }

    /// Whether `part` can contribute to a concat. `text_only` indicates the
    /// destination must contain valid UTF-8, which byte parts can satisfy only
    /// when they are inspectable at build time (literals, not sources).
    fn check_part(&self, part: &ValueSpec, text_only: bool) -> Result<()> {
        let ValueSpec::Leaf(leaf) = part else {
            bail!("a concat part must be a string or list<u8> value");
        };
        match leaf {
            Leaf::Str(_) => Ok(()),
            Leaf::Bytes(bytes) if text_only => match std::str::from_utf8(bytes) {
                Ok(_) => Ok(()),
                Err(error) => {
                    bail!("a list<u8> part joining a string must be valid UTF-8: {error}")
                }
            },
            Leaf::Bytes(_) => Ok(()),
            Leaf::Source(source) => {
                let resolve = self.ctx.resolve();
                if !is_byte_sequence(resolve, source.ty) {
                    bail!(
                        "a concat part must be a string or list<u8>, got {:?}",
                        source.ty
                    );
                }
                if text_only && !is_string(resolve, source.ty) {
                    bail!(
                        "a {:?} value's bytes are unknown at build time, so they \
                         cannot join a string, which must be valid UTF-8",
                        source.ty
                    );
                }
                Ok(())
            }
            other => bail!(
                "a concat part must be a string or list<u8>, got a {} literal \
                 (convert it with `format!` or an imported function)",
                leaf_kind_name(other)
            ),
        }
    }

    /// One pre-checked concat part's pointer and length, as locals.
    fn part_ptr_len(&self, part: &ValueSpec) -> Result<(u32, u32)> {
        let interned = match part {
            ValueSpec::Leaf(Leaf::Str(text)) => Some(self.ctx.intern(text.as_bytes())),
            ValueSpec::Leaf(Leaf::Bytes(bytes)) => Some(self.ctx.intern(bytes)),
            _ => None,
        };
        if let Some((offset, len)) = interned {
            let pointer = self.emitter.local(ValType::I32);
            let length = self.emitter.local(ValType::I32);
            self.emit(Instruction::I32Const(offset as i32));
            self.emit(Instruction::LocalSet(pointer));
            self.emit(Instruction::I32Const(len as i32));
            self.emit(Instruction::LocalSet(length));
            return Ok((pointer, length));
        }
        let ValueSpec::Leaf(Leaf::Source(source)) = part else {
            unreachable!("check_part accepts only literals and sources");
        };
        load_ptr_len(self.emitter, &source.slot)
    }

    /// Reserve `bytes` of heap and return the local holding the pointer.
    fn allocate(&self, bytes: usize) -> Local {
        let pointer = Local::new(self.emitter.local(ValType::I32), ValType::I32);
        call_allocator(self.ctx, self.emitter, Size::Const(bytes));
        self.emit(Instruction::LocalSet(pointer.index));
        pointer
    }

    /// Copy a materialized value's bytes into a memory slot.
    ///
    /// A memory source is copied verbatim, preserving the type's exact
    /// in-memory layout including padding: a variant with an 8-aligned payload
    /// after a 1-byte discriminant has gaps between its flats, and
    /// `memory.copy` of `size(ty)` bytes keeps them. A flat source has no
    /// padding; its flats are the value, so they are stored contiguously.
    fn copy_from(&self, source: &ValueRef, dest: &Slot) -> Result<()> {
        let (base, offset) = self.memory_dest(dest)?;
        match &source.slot {
            Slot::Memory {
                base: source_base,
                offset: source_offset,
            } => {
                self.emit(Instruction::LocalGet(base));
                if offset != 0 {
                    self.emit(Instruction::I32Const(offset as i32));
                    self.emit(Instruction::I32Add);
                }
                self.emit(Instruction::LocalGet(*source_base));
                if *source_offset != 0 {
                    self.emit(Instruction::I32Const(*source_offset as i32));
                    self.emit(Instruction::I32Add);
                }
                self.emit(Instruction::I32Const(
                    self.ctx.layout().size(&source.ty) as i32
                ));
                self.emit(Instruction::MemoryCopy {
                    src_mem: 0,
                    dst_mem: 0,
                });
                Ok(())
            }
            Slot::Flat { locals } => {
                let flats = abi::flat_types(self.ctx.resolve(), source.ty)?;
                let mut cursor = offset;
                for (local, flat) in locals.iter().zip(&flats) {
                    self.emit(Instruction::LocalGet(base));
                    self.emit(Instruction::LocalGet(local.index));
                    self.emit(Store::for_type(*flat)?.instruction(cursor));
                    cursor += flat_width(*flat)?;
                }
                Ok(())
            }
        }
    }

    /// Copy a materialized value into locals.
    ///
    /// The destination may be wider than the source: copying into a variant's
    /// payload means the locals are the join of every case, so a narrow one
    /// fills the leading slots and the rest stay zero; nothing reads past the
    /// case's own width. Hence `>`, not `!=`.
    fn copy_flat_from(&self, source: &ValueRef, dest_locals: &[Local]) -> Result<()> {
        let flats = abi::flat_types(self.ctx.resolve(), source.ty)?;
        if flats.len() > dest_locals.len() {
            bail!(
                "source flattens to {} core values but the destination has only {} locals",
                flats.len(),
                dest_locals.len()
            );
        }
        match &source.slot {
            Slot::Flat { locals } => {
                for (dest, source_local) in dest_locals.iter().zip(locals) {
                    self.set_local_from(*dest, *source_local)?;
                }
                Ok(())
            }
            Slot::Memory { base, offset } => {
                let mut cursor = *offset;
                for (dest, flat) in dest_locals.iter().zip(&flats) {
                    self.emit(Instruction::LocalGet(*base));
                    self.emit(Load::for_type(*flat)?.instruction(cursor));
                    self.set_local(*dest, *flat)?;
                    cursor += flat_width(*flat)?;
                }
                Ok(())
            }
        }
    }

    /// Write a `{ptr, len}` pair, the ABI form of `list`/`map`/`string`.
    ///
    /// The pair is the value and lives wherever the slot says; the elements
    /// live in the heap block at that pointer. The length is a number held
    /// while emitting, the pointer whatever the allocator returned at runtime,
    /// so the two halves are written differently.
    pub(crate) fn write_ptr_len(&self, slot: &Slot, pointer: Local, len: Len) -> Result<()> {
        match slot {
            Slot::Flat { locals } => {
                let [pointer_local, length_local, ..] = locals[..] else {
                    bail!("a list/map needs two locals (ptr, len)");
                };
                self.set_local_from(pointer_local, pointer)?;
                match len {
                    Len::Const(n) => self.set_local_const(length_local, n as i32),
                    Len::In(local) => self.set_local_from(length_local, local),
                }
            }
            Slot::Memory { base, offset } => {
                let (base, offset) = (*base, *offset);
                self.emit(Instruction::LocalGet(base));
                self.emit(Instruction::LocalGet(pointer.index));
                self.emit(Store::I32.instruction(offset));
                match len {
                    Len::Const(n) => self.store_const_i32(base, offset + 4, n as i32),
                    Len::In(local) => {
                        self.emit(Instruction::LocalGet(base));
                        self.emit(Instruction::LocalGet(local.index));
                        self.emit(Store::I32.instruction(offset + 4));
                        Ok(())
                    }
                }
            }
        }
    }

    /// Write a discriminant and return the slot its payload occupies, the
    /// split every `variant`/`option`/`result`/`enum` needs.
    ///
    /// Flattened, a variant is `[disc, ...joined_payload_flats]`, so the
    /// payload is everything after the first local. Returning the payload slot
    /// alone carries the joined widths a narrow case must bitcast up to, since
    /// those are the types of the payload locals.
    pub(crate) fn write_disc(
        &self,
        slot: &Slot,
        tag: Int,
        disc: i64,
        payload_offset: usize,
    ) -> Result<Slot> {
        match slot {
            Slot::Flat { locals } => {
                let Some(&disc_local) = locals.first() else {
                    bail!("a variant needs at least a discriminant local");
                };
                // A discriminant is a small non-negative integer, so it is an
                // i32 on the stack; the local may be wider from a join.
                self.set_local_const(disc_local, disc as i32)?;
                Ok(Slot::flat(locals[1..].to_vec()))
            }
            Slot::Memory { base, offset } => {
                let (base, offset) = (*base, *offset);
                self.store_disc(base, offset, tag, disc)?;
                Ok(Slot::Memory {
                    base,
                    offset: offset + payload_offset,
                })
            }
        }
    }

    /// Store an i32 constant at `base + offset`.
    fn store_const_i32(&self, base: u32, offset: usize, value: i32) -> Result<()> {
        self.emit(Instruction::LocalGet(base));
        self.emit(Instruction::I32Const(value));
        self.emit(Store::I32.instruction(offset));
        Ok(())
    }

    /// Store a discriminant at `base + offset`, narrowed to its tag width.
    fn store_disc(&self, base: u32, offset: usize, tag: Int, disc: i64) -> Result<()> {
        let store = Store::for_tag(tag);
        self.emit(Instruction::LocalGet(base));
        self.emit(match store.operand() {
            ValType::I64 => Instruction::I64Const(disc),
            _ => Instruction::I32Const(disc as i32),
        });
        self.emit(store.instruction(offset));
        Ok(())
    }

    /// Store a value already on the stack into `local`, converting it if the
    /// local was declared as a different type.
    ///
    /// `actual` is what is on the stack right now, which may not match the WIT
    /// type, e.g. a `u8` leaf pushes `i32.const`, so the stack holds an i32.
    fn set_local(&self, local: Local, actual: ValType) -> Result<()> {
        if local.ty != actual {
            for instruction in bitcast(actual, local.ty)? {
                self.emit(instruction);
            }
        }
        self.emit(Instruction::LocalSet(local.index));
        Ok(())
    }

    /// Set `local` to an i32 constant: an interned offset, a length, a flags
    /// word, a discriminant.
    fn set_local_const(&self, local: Local, value: i32) -> Result<()> {
        self.emit(Instruction::I32Const(value));
        self.set_local(local, ValType::I32)
    }

    /// Set `local` from another local, bitcasting if their types differ.
    ///
    /// A string's pointer is an offset known while emitting; a list's is
    /// whatever the allocator returns when the component runs.
    fn set_local_from(&self, local: Local, source: Local) -> Result<()> {
        self.emit(Instruction::LocalGet(source.index));
        self.set_local(local, source.ty)
    }
}

/// The alignment every allocation requests. 8 satisfies every wasm32 WIT type.
const ALIGN: i32 = 8;

/// How many bytes an allocation asks for: a size known while emitting, or one
/// the running component computes.
#[derive(Clone, Copy)]
pub(crate) enum Size {
    Const(usize),
    /// `count * stride` for a sequence whose length is only known at runtime.
    Strided {
        count: Local,
        stride: usize,
    },
}

/// Emit a call to the allocator, leaving the pointer on the stack. The
/// canonical ABI's realloc takes `(old_ptr, old_len, align, new_len)`, and a
/// fresh allocation passes zero for the first two.
pub(crate) fn call_allocator(ctx: &BuildContext, emitter: &Emitter, size: Size) {
    emitter.emit(Instruction::I32Const(0));
    emitter.emit(Instruction::I32Const(0));
    emitter.emit(Instruction::I32Const(ALIGN));
    match size {
        Size::Const(bytes) => emitter.emit(Instruction::I32Const(bytes as i32)),
        Size::Strided { count, stride } => {
            emitter.emit(Instruction::LocalGet(count.index));
            emitter.emit(Instruction::I32Const(stride as i32));
            emitter.emit(Instruction::I32Mul);
        }
    }
    emitter.emit(Instruction::Call(ctx.allocator()));
}

/// Reserve storage for a value of `ty`, returning the corresponding slot.
///
/// Flat when it can be: locals are declared in the function header and
/// zero-initialised, whereas memory requires allocation plus a store and load
/// per use.
///
/// The threshold is the ABI's own `MAX_FLAT_PARAMS`, since a value too wide to
/// cross a boundary flattened gains nothing from locals here. Composites
/// qualify too: the walks descend into a flat slot by sub-range exactly as
/// they descend by byte offset in memory.
///
/// A list's elements always live in a heap block, but its `{pointer, length}`
/// handle is two flats.
///
/// A zero-width type has no flats to occupy, so it takes the memory path.
pub(crate) fn reserve(ctx: &BuildContext, emitter: &Emitter, ty: wit_parser::Type) -> Result<Slot> {
    let flats = abi::flat_types(ctx.resolve(), ty)?;
    if flats.is_empty() || flats.len() > Resolve::MAX_FLAT_PARAMS {
        return Ok(reserve_memory(ctx, emitter, ty));
    }
    Ok(Slot::flat(
        flats
            .iter()
            .map(|flat| Local::new(emitter.local(*flat), *flat))
            .collect(),
    ))
}

/// Reserve linear memory for a value of `ty`: an allocation of its ABI size,
/// with the pointer in a local. The size is requested as-is. Alignment occurs
/// within the allocator, which rounds every address up to 8.
pub(crate) fn reserve_memory(ctx: &BuildContext, emitter: &Emitter, ty: wit_parser::Type) -> Slot {
    let base = emitter.local(ValType::I32);
    call_allocator(ctx, emitter, Size::Const(ctx.layout().size(&ty)));
    emitter.emit(Instruction::LocalSet(base));
    Slot::at(base)
}

/// Per-member slots for a record-like sequence. [`Slot::member`] takes
/// whatever the parent needs, so one computation serves either a memory or
/// flat destination. Used by both the read and write walks.
pub(crate) fn member_slots(
    ctx: &BuildContext,
    parent: &Slot,
    types: &[wit_parser::Type],
) -> Result<Vec<Slot>> {
    let offsets = ctx.layout().field_offsets(types.iter());
    let mut slots = Vec::with_capacity(types.len());
    let mut cursor = 0usize;
    for (ty, offset) in types.iter().zip(offsets) {
        let width = abi::flat_types(ctx.resolve(), *ty)?.len();
        slots.push(parent.member(offset, cursor, cursor + width));
        cursor += width;
    }
    Ok(slots)
}

/// Whether `ty` is `string`, following aliases. A string destination
/// constrains its parts, since the result must be valid UTF-8.
fn is_string(resolve: &Resolve, ty: wit_parser::Type) -> bool {
    match ty {
        wit_parser::Type::String => true,
        wit_parser::Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => is_string(resolve, *inner),
            _ => false,
        },
        _ => false,
    }
}

/// Whether `ty` is `string` or `list<u8>` (the types a concat produces), both
/// represented as `{ptr, len}` over bytes. Aliases are followed.
fn is_byte_sequence(resolve: &Resolve, ty: wit_parser::Type) -> bool {
    match ty {
        wit_parser::Type::String => true,
        wit_parser::Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Type(inner) => is_byte_sequence(resolve, *inner),
            TypeDefKind::List(elem) => matches!(elem, wit_parser::Type::U8),
            _ => false,
        },
        _ => false,
    }
}

/// The locals holding a `{ptr, len}` pair. In memory that is two loads;
/// flattened they are already locals.
pub(crate) fn load_ptr_len(emitter: &Emitter, slot: &Slot) -> Result<(u32, u32)> {
    match slot {
        Slot::Memory { base, offset } => {
            let pointer = emitter.local(ValType::I32);
            let length = emitter.local(ValType::I32);
            emitter.emit(Instruction::LocalGet(*base));
            emitter.emit(Load::I32.instruction(*offset));
            emitter.emit(Instruction::LocalSet(pointer));
            emitter.emit(Instruction::LocalGet(*base));
            emitter.emit(Load::I32.instruction(offset + 4));
            emitter.emit(Instruction::LocalSet(length));
            Ok((pointer, length))
        }
        Slot::Flat { locals } => {
            let [pointer, length, ..] = locals[..] else {
                bail!("a string/list/map needs two locals (ptr, len)");
            };
            Ok((pointer.index, length.index))
        }
    }
}

/// The bytes a core value occupies where flats are packed contiguously.
/// A width that does not match the type would misalign every flat after it.
fn flat_width(ty: ValType) -> Result<usize> {
    Ok(match ty {
        ValType::I32 | ValType::F32 => 4,
        ValType::I64 | ValType::F64 => 8,
        other => bail!("no flat width for {other:?}"),
    })
}

/// The WIT type name a literal was authored as, for the exact-match error,
/// which must name what the author wrote rather than what it lowers to.
fn leaf_kind_name(leaf: &Leaf) -> &'static str {
    match leaf {
        Leaf::Str(_) => "string",
        Leaf::Bytes(_) => "list<u8>",
        Leaf::Bool(_) => "bool",
        Leaf::S8(_) => "s8",
        Leaf::S16(_) => "s16",
        Leaf::S32(_) => "s32",
        Leaf::S64(_) => "s64",
        Leaf::U8(_) => "u8",
        Leaf::U16(_) => "u16",
        Leaf::U32(_) => "u32",
        Leaf::U64(_) => "u64",
        Leaf::F32(_) => "f32",
        Leaf::F64(_) => "f64",
        Leaf::Char(_) => "char",
        Leaf::Concat(_) => "concat",
        Leaf::Source(_) => "source",
    }
}

/// Which core constant a leaf becomes, and the core type it produces.
///
/// The WIT types must match exactly: a `u8` spec is only valid at a `u8`
/// position. Neither widening nor core-type aliasing is accepted, so `s32`
/// into a `u32` slot is an error even though both are core i32.
fn push_scalar(leaf: &Leaf, dest: wit_parser::Type) -> Result<(Instruction<'static>, ValType)> {
    use wit_parser::Type as T;
    Ok(match (leaf, dest) {
        (Leaf::Bool(v), T::Bool) => (Instruction::I32Const(i32::from(*v)), ValType::I32),
        (Leaf::S8(v), T::S8) => (Instruction::I32Const(*v as i32), ValType::I32),
        (Leaf::S16(v), T::S16) => (Instruction::I32Const(*v as i32), ValType::I32),
        (Leaf::S32(v), T::S32) => (Instruction::I32Const(*v), ValType::I32),
        (Leaf::S64(v), T::S64) => (Instruction::I64Const(*v), ValType::I64),
        (Leaf::U8(v), T::U8) => (Instruction::I32Const(*v as i32), ValType::I32),
        (Leaf::U16(v), T::U16) => (Instruction::I32Const(*v as i32), ValType::I32),
        (Leaf::U32(v), T::U32) => (Instruction::I32Const(*v as i32), ValType::I32),
        (Leaf::U64(v), T::U64) => (Instruction::I64Const(*v as i64), ValType::I64),
        (Leaf::F32(v), T::F32) => (Instruction::F32Const((*v).into()), ValType::F32),
        (Leaf::F64(v), T::F64) => (Instruction::F64Const((*v).into()), ValType::F64),
        (Leaf::Char(v), T::Char) => (Instruction::I32Const(*v as i32), ValType::I32),
        (leaf, dest) => bail!(
            "a {} literal cannot be written to a {dest:?} position",
            leaf_kind_name(leaf)
        ),
    })
}

/// A `flags` value's bitset words, resolved from the named flags that are set.
///
/// Flag `i` is bit `i % 32` of word `i / 32`; the repr is one word for up to
/// 16 flags, else `ceil(n/32)` of them. Shared by both memory and flat slots,
/// so the two paths cannot drift on which names are valid.
///
/// A flags type with no flags occupies no bytes, so it yields no words. The
/// count here must track `Flags::repr`, which returns `U32(0)` for that case:
/// handing back a word for a zero-width type would write past the value.
fn flag_words(flags: &wit_parser::Flags, set: &[String]) -> Result<Vec<u32>> {
    let count = flags.flags.len();
    let word_count = match count {
        0 => 0,
        n if n <= 16 => 1,
        n => n.div_ceil(32),
    };
    let mut words = vec![0u32; word_count];
    for name in set {
        let index = flags
            .flags
            .iter()
            .position(|flag| &flag.name == name)
            .ok_or_else(|| anyhow!("no flag '{name}'"))?;
        words[index / 32] |= 1u32 << (index % 32);
    }
    Ok(words)
}

/// A leaf load and the core type it produces.
#[derive(Clone, Copy)]
pub(crate) enum Load {
    I32,
    I32From8,
    I32From16,
    I64,
    F32,
    F64,
}

impl Load {
    /// The load for a discriminant of this width.
    pub(crate) fn for_tag(tag: Int) -> Load {
        match tag {
            Int::U8 => Load::I32From8,
            Int::U16 => Load::I32From16,
            Int::U32 => Load::I32,
            Int::U64 => Load::I64,
        }
    }

    /// The full-width load for a core type.
    fn for_type(ty: ValType) -> Result<Load> {
        Ok(match ty {
            ValType::I32 => Load::I32,
            ValType::I64 => Load::I64,
            ValType::F32 => Load::F32,
            ValType::F64 => Load::F64,
            other => bail!("no load for {other:?}"),
        })
    }

    /// The type this load leaves on the stack.
    fn result(self) -> ValType {
        match self {
            Load::I32 | Load::I32From8 | Load::I32From16 => ValType::I32,
            Load::I64 => ValType::I64,
            Load::F32 => ValType::F32,
            Load::F64 => ValType::F64,
        }
    }

    pub(crate) fn instruction(self, offset: usize) -> Instruction<'static> {
        let memory = |align| MemArg {
            offset: offset as u64,
            align,
            memory_index: 0,
        };
        match self {
            Load::I32 => Instruction::I32Load(memory(2)),
            Load::I32From8 => Instruction::I32Load8U(memory(0)),
            Load::I32From16 => Instruction::I32Load16U(memory(1)),
            Load::I64 => Instruction::I64Load(memory(3)),
            Load::F32 => Instruction::F32Load(memory(2)),
            Load::F64 => Instruction::F64Load(memory(3)),
        }
    }
}

/// A leaf store, mirroring [`Load`].
///
/// A discriminant narrows to its declared width, so the tag widths are here
/// alongside the full-width stores. One type covers both, as on the read side.
#[derive(Clone, Copy)]
enum Store {
    I32,
    I32To8,
    I32To16,
    I64,
    F32,
    F64,
}

impl Store {
    /// The store for a discriminant of this width.
    fn for_tag(tag: Int) -> Store {
        match tag {
            Int::U8 => Store::I32To8,
            Int::U16 => Store::I32To16,
            Int::U32 => Store::I32,
            Int::U64 => Store::I64,
        }
    }

    /// The full-width store for a core type.
    fn for_type(ty: ValType) -> Result<Store> {
        Ok(match ty {
            ValType::I32 => Store::I32,
            ValType::I64 => Store::I64,
            ValType::F32 => Store::F32,
            ValType::F64 => Store::F64,
            other => bail!("no store for {other:?}"),
        })
    }

    /// The core type this store takes off the stack: a narrowing store still
    /// consumes an i32 and drops the high bits.
    fn operand(self) -> ValType {
        match self {
            Store::I32 | Store::I32To8 | Store::I32To16 => ValType::I32,
            Store::I64 => ValType::I64,
            Store::F32 => ValType::F32,
            Store::F64 => ValType::F64,
        }
    }

    fn instruction(self, offset: usize) -> Instruction<'static> {
        let memory = |align| MemArg {
            offset: offset as u64,
            align,
            memory_index: 0,
        };
        match self {
            Store::I32 => Instruction::I32Store(memory(2)),
            Store::I32To8 => Instruction::I32Store8(memory(0)),
            Store::I32To16 => Instruction::I32Store16(memory(1)),
            Store::I64 => Instruction::I64Store(memory(3)),
            Store::F32 => Instruction::F32Store(memory(2)),
            Store::F64 => Instruction::F64Store(memory(3)),
        }
    }
}

/// A zero of the given core type, for padding a variant arm narrower than the
/// joined width.
fn zero(ty: ValType) -> Instruction<'static> {
    match ty {
        ValType::I64 => Instruction::I64Const(0),
        ValType::F32 => Instruction::F32Const(0.0.into()),
        ValType::F64 => Instruction::F64Const(0.0.into()),
        _ => Instruction::I32Const(0),
    }
}

/// Convert a core value on the stack from one type to another.
///
/// A variant's payload slots are the join of every case, so a narrow case must
/// widen into them. Wasm has no single instruction for it, e.g. `f32` to `i64`
/// is a reinterpret and then an extend, so this returns a sequence.
fn bitcast(from: ValType, to: ValType) -> Result<Vec<Instruction<'static>>> {
    use Instruction as Op;
    use ValType::{F32, F64, I32, I64};
    Ok(match (from, to) {
        (from, to) if from == to => Vec::new(),
        (I32, I64) => vec![Op::I64ExtendI32U],
        (F32, I32) => vec![Op::I32ReinterpretF32],
        (F64, I64) => vec![Op::I64ReinterpretF64],
        (F32, I64) => vec![Op::I32ReinterpretF32, Op::I64ExtendI32U],
        (I64, I32) => vec![Op::I32WrapI64],
        (I32, F32) => vec![Op::F32ReinterpretI32],
        (I64, F64) => vec![Op::F64ReinterpretI64],
        (I64, F32) => vec![Op::I32WrapI64, Op::F32ReinterpretI32],
        (from, to) => bail!("cannot convert {from:?} to {to:?}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Opcodes, for the assertions that read an emitted body.
    const I32_STORE: u8 = 0x36;
    const I64_STORE: u8 = 0x37;
    const I32_STORE8: u8 = 0x3A;
    const I32_STORE16: u8 = 0x3B;
    const I32_ADD: u8 = 0x6A;

    fn context(wit: &str) -> BuildContext {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("test.wit", wit).expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");
        BuildContext::new(Rc::new(resolve), world)
    }

    const WORLD: &str = r"package test:ctx;
        interface host { log: func(); }
        world w { import host; export run: func(); }";

    #[test]
    fn imports_are_indexed_before_the_allocator() {
        let ctx = context(WORLD);
        assert_eq!(ctx.imports().len(), 1);
        assert_eq!(ctx.allocator(), 1);
    }

    #[test]
    fn identical_strings_share_one_offset() {
        let ctx = context(WORLD);
        assert_eq!(ctx.intern(b"hello"), (0, 5));
        assert_eq!(ctx.intern(b"world"), (5, 5));
        assert_eq!(ctx.intern(b"hello"), (0, 5));
    }

    #[test]
    fn identical_func_types_share_one_index() {
        let ctx = context(WORLD);
        assert_eq!(ctx.func_type(&[ValType::I32], &[]), 0);
        assert_eq!(ctx.func_type(&[], &[ValType::I32]), 1);
        assert_eq!(ctx.func_type(&[ValType::I32], &[]), 0);
    }

    #[test]
    fn interning_releases_its_borrow() {
        let ctx = context(WORLD);
        // Each call must leave the cell free for the next, including from
        // inside a body that is mid-emission.
        ctx.intern(b"first");
        ctx.func_type(&[], &[]);
        ctx.intern(b"second");
        assert_eq!(ctx.intern(b"first"), (0, 5));
    }

    #[test]
    fn taking_the_module_state_yields_what_was_interned() {
        let ctx = context(WORLD);
        ctx.intern(b"hello");
        ctx.func_type(&[ValType::I32], &[]);
        let (_, strings) = ctx.take_module_state();
        assert_eq!(strings.len(), 5);
        // The context is left empty, so interning again starts over.
        assert_eq!(ctx.intern(b"hello"), (0, 5));
        assert_eq!(ctx.func_type(&[ValType::I32], &[]), 0);
    }

    #[test]
    fn a_layout_is_available_for_the_worlds_types() {
        let ctx = context(WORLD);
        assert_eq!(ctx.layout().size(&wit_parser::Type::U64), 8);
    }

    #[test]
    fn a_memory_slot_starts_at_its_base() {
        let slot = Slot::at(3);
        assert_eq!(slot.base(), Some(3));
        assert_eq!(slot.offset(), 0);
        assert!(slot.locals().is_empty());
    }

    #[test]
    fn a_flat_slot_has_no_base() {
        let slot = Slot::flat(vec![
            Local::new(0, ValType::I32),
            Local::new(1, ValType::I64),
        ]);
        assert_eq!(slot.base(), None);
        assert_eq!(slot.offset(), 0);
        assert_eq!(slot.locals().len(), 2);
    }

    #[test]
    fn a_memory_member_is_offset_from_the_same_base() {
        let member = Slot::at(3).member(8, 0, 0);
        assert_eq!(member.base(), Some(3));
        assert_eq!(member.offset(), 8);
    }

    #[test]
    fn a_flat_member_is_a_sub_range_of_the_locals() {
        let locals = vec![
            Local::new(0, ValType::I32),
            Local::new(1, ValType::I64),
            Local::new(2, ValType::F32),
        ];
        let member = Slot::flat(locals).member(0, 1, 3);
        let types: Vec<ValType> = member.locals().iter().map(|l| l.ty).collect();
        assert_eq!(types, vec![ValType::I64, ValType::F32]);
        assert_eq!(member.locals()[0].index, 1);
    }

    #[test]
    fn nested_members_accumulate_their_offsets() {
        let member = Slot::at(0).member(8, 0, 0).member(4, 0, 0);
        assert_eq!(member.offset(), 12);
    }

    #[test]
    fn a_leaf_spec_carries_its_wit_type() {
        assert!(matches!(ValueSpec::u8(7), ValueSpec::Leaf(Leaf::U8(7))));
        assert!(matches!(ValueSpec::s64(-1), ValueSpec::Leaf(Leaf::S64(-1))));
        // A u8 and a u32 are different specs even though both are core i32.
        assert!(matches!(ValueSpec::u32(7), ValueSpec::Leaf(Leaf::U32(7))));
    }

    #[test]
    fn an_optional_string_is_some_or_none() {
        assert!(matches!(
            ValueSpec::optional_string(Some("x")),
            ValueSpec::Option(Some(_))
        ));
        assert!(matches!(
            ValueSpec::optional_string(None::<&str>),
            ValueSpec::Option(None)
        ));
    }

    #[test]
    fn a_variant_case_may_carry_a_payload_or_not() {
        let ValueSpec::Variant { case, payload } = ValueSpec::variant("full", ValueSpec::u32(1))
        else {
            panic!("expected a variant");
        };
        assert_eq!(case, "full");
        assert!(payload.is_some());

        let ValueSpec::Variant { case, payload } = ValueSpec::variant_unit("empty") else {
            panic!("expected a variant");
        };
        assert_eq!(case, "empty");
        assert!(payload.is_none());
    }

    #[test]
    fn a_record_spec_keeps_its_fields_in_order() {
        let ValueSpec::Record(fields) =
            ValueSpec::record([("x", ValueSpec::u32(1)), ("y", ValueSpec::u32(2))])
        else {
            panic!("expected a record");
        };
        let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["x", "y"]);
    }

    /// Load the named type from a pointer, and check that the body validates
    /// with the flats it claims to leave on the stack.
    fn loads(wit: &str, type_name: &str) -> Vec<ValType> {
        let ctx = context(wit);
        let ty = named_type(&ctx, type_name);
        // One param: the pointer the value is read from.
        let emitter = Emitter::new(1);
        let loader = Loader::new(&ctx, &emitter);
        let flats = loader.load(ty, 0).expect("load");
        let function = emitter.encode().expect("encode");
        validate(&ctx, function, vec![ValType::I32], flats.clone());
        flats
    }

    /// The named type of the sole interface.
    fn named_type(ctx: &BuildContext, name: &str) -> wit_parser::Type {
        let id = ctx.resolve().worlds[ctx.world()]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    ctx.resolve().interfaces[*id].types.get(name).copied()
                }
                _ => None,
            })
            .expect("the declared type");
        wit_parser::Type::Id(id)
    }

    /// Wrap a body in a module with one memory and validate it.
    fn validate(
        ctx: &BuildContext,
        function: wasm_encoder::Function,
        params: Vec<ValType>,
        results: Vec<ValType>,
    ) {
        let (types, strings) = ctx.take_module_state();
        let module = crate::module::CoreModule {
            imports: Vec::new(),
            functions: vec![crate::module::CoreFunction {
                params,
                results,
                body: function,
                export_name: "load".to_string(),
            }],
            memories: vec![crate::module::CoreMemory {
                ty: wasm_encoder::MemoryType {
                    minimum: 1,
                    maximum: None,
                    memory64: false,
                    shared: false,
                    page_size_log2: None,
                },
                export_name: None,
            }],
            globals: Vec::new(),
            types,
            data: strings,
        };
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&module.encode())
            .expect("the emitted loads must be valid wasm");
    }

    #[test]
    fn a_record_loads_each_field() {
        let flats = loads(
            r"package test:rec;
              interface i { record point { x: u32, y: u64 } f: func(p: point); }
              world w { import i; }",
            "point",
        );
        assert_eq!(flats, vec![ValType::I32, ValType::I64]);
    }

    #[test]
    fn a_list_loads_its_pointer_and_length() {
        let flats = loads(
            r"package test:lst;
              interface i { type items = list<u32>; f: func(x: items); }
              world w { import i; }",
            "items",
        );
        assert_eq!(flats, vec![ValType::I32, ValType::I32]);
    }

    #[test]
    fn a_variant_loads_a_discriminant_and_a_joined_payload() {
        let flats = loads(
            r"package test:var;
              interface i { variant v { small(u32), wide(u64) } f: func(x: v); }
              world w { import i; }",
            "v",
        );
        // The discriminant, then one slot wide enough for the u64 case.
        assert_eq!(flats, vec![ValType::I32, ValType::I64]);
    }

    #[test]
    fn a_narrow_variant_case_widens_into_the_joined_slot() {
        // The string case is two flats where the f64 case is one 64-bit slot;
        // so the pointer must widen, and the length occupies the second.
        let flats = loads(
            r"package test:widen;
              interface i { variant v { text(string), wide(f64) } f: func(x: v); }
              world w { import i; }",
            "v",
        );
        assert_eq!(flats, vec![ValType::I32, ValType::I64, ValType::I32]);
    }

    #[test]
    fn an_enum_loads_only_its_discriminant() {
        let flats = loads(
            r"package test:enm;
              interface i { enum color { red, green } f: func(c: color); }
              world w { import i; }",
            "color",
        );
        assert_eq!(flats, vec![ValType::I32]);
    }

    #[test]
    fn a_handle_loads_as_one_i32() {
        let flats = loads(
            r"package test:hnd;
              interface i { resource conn; type link = borrow<conn>; f: func(c: link); }
              world w { import i; }",
            "link",
        );
        assert_eq!(flats, vec![ValType::I32]);
    }

    /// Emit a write of `value` into a fresh memory area and validate the body.
    ///
    /// The allocator is declared as an import so the list/map paths, which call
    /// it, produce a module that type-checks.
    fn writes_memory(wit: &str, type_name: &str, value: &ValueSpec) -> Vec<u8> {
        let ctx = context(wit);
        let ty = named_type(&ctx, type_name);
        let emitter = Emitter::new(1);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), value)
            .expect("write");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32], Vec::new())
    }

    /// Emit a write into locals of the given core types, and validate.
    fn writes_flat(
        wit: &str,
        type_name: &str,
        declared: &[ValType],
        value: &ValueSpec,
    ) -> Result<Vec<u8>> {
        let ctx = context(wit);
        let ty = named_type(&ctx, type_name);
        let emitter = Emitter::new(0);
        let locals: Vec<Local> = declared
            .iter()
            .map(|core| Local::new(emitter.local(*core), *core))
            .collect();
        Writer::new(&ctx, &emitter).write(ty, &Slot::flat(locals), value)?;
        let function = emitter.encode().expect("encode");
        Ok(validate_with_allocator(
            &ctx,
            function,
            Vec::new(),
            Vec::new(),
        ))
    }

    /// Like [`validate`], but with the allocator import every list/map write
    /// calls. Returns the encoded module.
    fn validate_with_allocator(
        ctx: &BuildContext,
        function: wasm_encoder::Function,
        params: Vec<ValType>,
        results: Vec<ValType>,
    ) -> Vec<u8> {
        let (types, strings) = ctx.take_module_state();
        // The allocator sits at the index the context reports, so the imports
        // before it are padded out to put it there.
        let allocator = ctx.allocator() as usize;
        let mut imports: Vec<crate::module::CoreImport> = (0..allocator)
            .map(|index| crate::module::CoreImport {
                module: "test".to_string(),
                name: format!("pad{index}"),
                params: Vec::new(),
                results: Vec::new(),
            })
            .collect();
        imports.push(crate::module::CoreImport {
            module: "test".to_string(),
            name: "alloc".to_string(),
            params: vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            results: vec![ValType::I32],
        });
        let module = crate::module::CoreModule {
            imports,
            functions: vec![crate::module::CoreFunction {
                params,
                results,
                body: function,
                export_name: "write".to_string(),
            }],
            memories: vec![crate::module::CoreMemory {
                ty: wasm_encoder::MemoryType {
                    minimum: 1,
                    maximum: None,
                    memory64: false,
                    shared: false,
                    page_size_log2: None,
                },
                export_name: None,
            }],
            globals: Vec::new(),
            types,
            data: strings,
        };
        let bytes = module.encode();
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("the emitted writes must be valid wasm");
        bytes
    }

    const RECORD_WIT: &str = r"package test:writerec;
        interface i { record point { x: u32, y: u64 } f: func(p: point); }
        world w { import i; }";

    #[test]
    fn a_record_writes_each_field_at_its_offset() {
        writes_memory(
            RECORD_WIT,
            "point",
            &ValueSpec::record([("x", ValueSpec::u32(7)), ("y", ValueSpec::u64(9))]),
        );
    }

    #[test]
    fn a_record_rejects_a_field_it_does_not_declare() {
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(1);
        let error = Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::at(0),
                &ValueSpec::record([
                    ("x", ValueSpec::u32(1)),
                    ("y", ValueSpec::u64(2)),
                    ("z", ValueSpec::u32(3)),
                ]),
            )
            .expect_err("an undeclared field must be rejected");
        let text = format!("{error:#}");
        assert!(text.contains("'z'"), "names the unknown field: {text}");
    }

    #[test]
    fn a_record_rejects_a_missing_field() {
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(1);
        let error = Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::at(0),
                &ValueSpec::record([("x", ValueSpec::u32(1))]),
            )
            .expect_err("a missing field must be rejected");
        let text = format!("{error:#}");
        assert!(text.contains("'y'"), "names the absent field: {text}");
    }

    #[test]
    fn a_literal_must_match_its_declared_type_exactly() {
        // An `s32` literal in a `u32` field: both are core i32, so wasm
        // validation would accept it. Rejected by a WIT-level check.
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(1);
        let error = Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::at(0),
                &ValueSpec::record([("x", ValueSpec::s32(7)), ("y", ValueSpec::u64(9))]),
            )
            .expect_err("an s32 literal cannot fill a u32 field");
        let text = format!("{error:#}");
        assert!(text.contains("s32"), "names what the author wrote: {text}");
    }

    #[test]
    fn a_string_writes_its_interned_pointer_and_length() {
        let ctx = context(
            r"package test:writestr;
              interface i { type name = string; f: func(n: name); }
              world w { import i; }",
        );
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(1);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &ValueSpec::string("hello"))
            .expect("write");
        let function = emitter.encode().expect("encode");
        let bytes = validate_with_allocator(&ctx, function, vec![ValType::I32], Vec::new());
        // The literal reached the data segment, so the pointer stored is real.
        assert!(
            bytes.windows(5).any(|window| window == b"hello"),
            "the interned string is in the data segment"
        );
    }

    #[test]
    fn a_list_allocates_backing_storage_then_writes_pointer_and_length() {
        writes_memory(
            r"package test:writelist;
              interface i { type bytes = list<u8>; f: func(b: bytes); }
              world w { import i; }",
            "bytes",
            &ValueSpec::list([ValueSpec::u8(1), ValueSpec::u8(2), ValueSpec::u8(3)]),
        );
    }

    #[test]
    fn a_variant_writes_its_discriminant_and_payload() {
        writes_memory(
            r"package test:writevar;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
            "shape",
            &ValueSpec::variant("circle", ValueSpec::u32(4)),
        );
    }

    #[test]
    fn a_variant_case_without_a_payload_writes_only_its_discriminant() {
        writes_memory(
            r"package test:writeunit;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
            "shape",
            &ValueSpec::variant_unit("empty"),
        );
    }

    #[test]
    fn a_variant_rejects_a_payload_its_case_does_not_take() {
        let ctx = context(
            r"package test:writebadvar;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
        );
        let ty = named_type(&ctx, "shape");
        let emitter = Emitter::new(1);
        let error = Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::at(0),
                &ValueSpec::variant("empty", ValueSpec::u32(1)),
            )
            .expect_err("a unit case takes no payload");
        assert!(format!("{error:#}").contains("takes no payload"));
    }

    #[test]
    fn a_narrow_case_fills_only_the_leading_locals_of_a_joined_slot() {
        // `text` flattens to (i32, i32); `wide` to a single i64. The joined
        // payload is therefore (i64, i32), and writing the narrow case must
        // bitcast its first flat up to i64 and leave the second local alone.
        let wit = r"package test:joinwrite;
              interface i { variant v { text(string), wide(u64) } f: func(x: v); }
              world w { import i; }";
        writes_flat(
            wit,
            "v",
            &[ValType::I32, ValType::I64, ValType::I32],
            &ValueSpec::variant("wide", ValueSpec::u64(5)),
        )
        .expect("the narrow case widens into the joined slot");
    }

    #[test]
    fn a_flat_scalar_is_reconciled_against_its_declared_local() {
        // A u32 pushes an i32, but the local was declared i64.
        // Without the bitcast the body would fail to validate.
        writes_flat(
            r"package test:reconcile;
              interface i { type n = u32; f: func(x: n); }
              world w { import i; }",
            "n",
            &[ValType::I64],
            &ValueSpec::u32(3),
        )
        .expect("the scalar widens to its local's declared type");
    }

    #[test]
    fn a_composite_cannot_be_written_into_locals() {
        let error = writes_flat(
            RECORD_WIT,
            "point",
            &[ValType::I32, ValType::I64],
            &ValueSpec::u32(1),
        )
        .expect_err("a record needs a memory destination");
        assert!(format!("{error:#}").contains("Record"));
    }

    #[test]
    fn flags_set_the_bits_they_name() {
        writes_memory(
            r"package test:writeflags;
              interface i { flags perms { read, write, exec } f: func(p: perms); }
              world w { import i; }",
            "perms",
            &ValueSpec::flags(["read", "exec"]),
        );
    }

    #[test]
    fn each_flag_sets_the_bit_at_its_declared_position() {
        let ctx = context(
            r"package test:flagbits;
              interface i { flags perms { read, write, exec } f: func(p: perms); }
              world w { import i; }",
        );
        let flags = match &ctx.resolve().types[match named_type(&ctx, "perms") {
            wit_parser::Type::Id(id) => id,
            other => panic!("expected a type id, got {other:?}"),
        }]
        .kind
        {
            TypeDefKind::Flags(flags) => flags.clone(),
            other => panic!("expected flags, got {other:?}"),
        };
        // Flag i is bit i, so read|exec is 0b101.
        assert_eq!(
            flag_words(&flags, &["read".into(), "exec".into()]).expect("words"),
            vec![0b101]
        );
        assert_eq!(
            flag_words(&flags, &["write".into()]).expect("words"),
            vec![0b010]
        );
        assert_eq!(flag_words(&flags, &[]).expect("words"), vec![0]);
    }

    #[test]
    fn a_flag_past_the_first_word_sets_a_bit_in_its_own_word() {
        let ctx = context(
            r"package test:flagwords;
              interface i {
                flags wide {
                  b00, b01, b02, b03, b04, b05, b06, b07, b08, b09, b10, b11,
                  b12, b13, b14, b15, b16, b17, b18, b19, b20, b21, b22, b23,
                  b24, b25, b26, b27, b28, b29, b30, b31, b32, b33
                }
                check: func(w: wide);
              }
              world w { import i; }",
        );
        let flags = match &ctx.resolve().types[match named_type(&ctx, "wide") {
            wit_parser::Type::Id(id) => id,
            other => panic!("expected a type id, got {other:?}"),
        }]
        .kind
        {
            TypeDefKind::Flags(flags) => flags.clone(),
            other => panic!("expected flags, got {other:?}"),
        };
        // 34 flags need two words; b33 is bit 1 of word 1.
        let words = flag_words(&flags, &["b00".into(), "b33".into()]).expect("words");
        assert_eq!(words, vec![0b1, 0b10]);
    }

    #[test]
    fn a_body_that_allocates_assembles_into_a_valid_module() {
        // Unlike the harness tests, this assembles the real module, so the
        // allocator call site is checked against the real declared signature.
        // The export declares no result so the body owes nothing on the stack,
        // and the test is only about the allocation it performs internally.
        let mut resolve = Resolve::new();
        let package = resolve
            .push_str(
                "test.wit",
                r"package test:allocreal;
                  interface i { type nums = list<u32>; }
                  world w { import i; export run: func(); }",
            )
            .expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");
        let ctx = Rc::new(BuildContext::new(Rc::new(resolve.clone()), world));
        let emitter = Emitter::new(0);
        let list = resolve.worlds[world]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].types.get("nums").copied()
                }
                _ => None,
            })
            .map(wit_parser::Type::Id)
            .expect("the declared list type");
        // Writing a list allocates its storage, which calls the allocator.
        let slot = reserve(&ctx, &emitter, list).expect("reserve");
        Writer::new(&ctx, &emitter)
            .write(list, &slot, &ValueSpec::list([ValueSpec::u32(1)]))
            .expect("write");
        let body = emitter.encode().expect("encode");
        let (types, data) = ctx.take_module_state();
        let module = abi::core_module(
            &resolve,
            world,
            vec![abi::GeneratedFunction {
                interface: None,
                func: "run".to_string(),
                body,
            }],
            types,
            data,
        )
        .expect("assemble");
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&module.encode())
            .expect("the assembled module must be valid wasm");
    }

    #[test]
    fn a_zero_width_value_is_never_written_into() {
        // `flags { }`, `record { }` and `tuple<>` all occupy no bytes, so
        // their allocation is zero-sized. Any store into one would write past
        // the value, into whatever the allocator hands out next: silent
        // corruption that would both validate and run.
        let ctx = context(
            r"package test:zerowidth;
              interface i {
                flags noflags { }
                record norec { }
                type notuple = tuple<>;
                f: func(a: u32);
              }
              world w { import i; }",
        );
        for (name, spec) in [
            ("noflags", ValueSpec::flags(Vec::<String>::new())),
            (
                "norec",
                ValueSpec::record(Vec::<(String, ValueSpec)>::new()),
            ),
            ("notuple", ValueSpec::tuple(Vec::<ValueSpec>::new())),
        ] {
            let ty = named_type(&ctx, name);
            assert_eq!(ctx.layout().size(&ty), 0, "{name} occupies no bytes");
            let emitter = Emitter::new(0);
            let slot = reserve(&ctx, &emitter, ty).expect("reserve");
            Writer::new(&ctx, &emitter)
                .write(ty, &slot, &spec)
                .expect("write");
            let bytes = emitter.encode().expect("encode").into_raw_body();
            let stores = bytes
                .iter()
                .filter(|byte| matches!(**byte, I32_STORE | I32_STORE8 | I32_STORE16 | I64_STORE))
                .count();
            assert_eq!(stores, 0, "{name} emits no store: {bytes:02x?}");
        }
    }

    #[test]
    fn a_flags_type_with_no_flags_yields_no_words() {
        // `Flags::repr` returns `U32(0)` for zero flags, so the word count
        // must agree: handing back a word for a zero-width type would produce
        // an out-of-bounds store.
        let ctx = context(
            r"package test:noflagwords;
              interface i { flags empty { } f: func(e: empty); }
              world w { import i; }",
        );
        let flags = match &ctx.resolve().types[match named_type(&ctx, "empty") {
            wit_parser::Type::Id(id) => id,
            other => panic!("expected a type id, got {other:?}"),
        }]
        .kind
        {
            TypeDefKind::Flags(flags) => flags.clone(),
            other => panic!("expected flags, got {other:?}"),
        };
        assert!(flag_words(&flags, &[]).expect("words").is_empty());
    }

    #[test]
    fn flags_reject_a_name_they_do_not_declare() {
        let ctx = context(
            r"package test:writebadflags;
              interface i { flags perms { read, write } f: func(p: perms); }
              world w { import i; }",
        );
        let ty = named_type(&ctx, "perms");
        let emitter = Emitter::new(1);
        let error = Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &ValueSpec::flags(["exec"]))
            .expect_err("an undeclared flag must be rejected");
        assert!(format!("{error:#}").contains("'exec'"));
    }

    #[test]
    fn an_enum_writes_only_its_discriminant() {
        writes_memory(
            r"package test:writeenum;
              interface i { enum color { red, green } f: func(c: color); }
              world w { import i; }",
            "color",
            &ValueSpec::variant_unit("green"),
        );
    }

    #[test]
    fn tuple_members_are_laid_out_positionally() {
        writes_memory(
            r"package test:writetuple;
              interface i { type pair = tuple<u32, u64>; f: func(p: pair); }
              world w { import i; }",
            "pair",
            &ValueSpec::tuple([ValueSpec::u32(1), ValueSpec::u64(2)]),
        );
    }

    #[test]
    fn a_tuple_rejects_the_wrong_member_count() {
        let ctx = context(
            r"package test:writebadtuple;
              interface i { type pair = tuple<u32, u64>; f: func(p: pair); }
              world w { import i; }",
        );
        let ty = named_type(&ctx, "pair");
        let emitter = Emitter::new(1);
        let error = Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &ValueSpec::tuple([ValueSpec::u32(1)]))
            .expect_err("a tuple needs every member");
        assert!(format!("{error:#}").contains("expects 2"));
    }

    #[test]
    fn an_option_writes_a_discriminant_for_each_case() {
        let wit = r"package test:writeopt;
              interface i { type maybe = option<u32>; f: func(m: maybe); }
              world w { import i; }";
        writes_memory(wit, "maybe", &ValueSpec::none());
        writes_memory(wit, "maybe", &ValueSpec::some(ValueSpec::u32(1)));
    }

    #[test]
    fn a_result_writes_the_payload_of_the_case_it_names() {
        let wit = r"package test:writeres;
              interface i { type outcome = result<u32, string>; f: func(o: outcome); }
              world w { import i; }";
        writes_memory(wit, "outcome", &ValueSpec::ok(ValueSpec::u32(1)));
        writes_memory(wit, "outcome", &ValueSpec::err(ValueSpec::string("no")));
    }

    #[test]
    fn a_map_is_written_as_a_list_of_pairs() {
        writes_memory(
            r"package test:writemap;
              interface i { type table = map<string, u32>; f: func(t: table); }
              world w { import i; }",
            "table",
            &ValueSpec::map([(ValueSpec::string("a"), ValueSpec::u32(1))]),
        );
    }

    const STRING_WIT: &str = r"package test:concat;
        interface i { type name = string; f: func(n: name); }
        world w { import i; }";

    /// A string source at the given slot, for joining with other parts.
    fn string_source(ctx: &BuildContext, slot: Slot) -> ValueSpec {
        ValueSpec::source(ValueRef {
            ty: named_type(ctx, "name"),
            slot,
        })
    }

    #[test]
    fn joining_only_literals_interns_one_string() {
        // Values all known at build time, so parts collapse to a literal.
        let spec = ValueSpec::concat([ValueSpec::string("hello "), ValueSpec::string("world")]);
        let ValueSpec::Leaf(Leaf::Str(text)) = spec else {
            panic!("expected a single interned literal");
        };
        assert_eq!(text, "hello world");
    }

    #[test]
    fn joining_a_concat_flattens_its_parts() {
        let ctx = context(STRING_WIT);
        let inner = ValueSpec::concat([ValueSpec::string("a"), string_source(&ctx, Slot::at(0))]);
        let spec = ValueSpec::concat([inner, ValueSpec::string("b")]);
        let ValueSpec::Leaf(Leaf::Concat(parts)) = spec else {
            panic!("expected a concat");
        };
        assert_eq!(parts.len(), 3);
        assert!(matches!(parts[1], ValueSpec::Leaf(Leaf::Source(_))));
    }

    #[test]
    fn a_literal_joined_to_a_ref_in_memory_is_allocated_and_copied() {
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(2);
        let spec = ValueSpec::concat([
            ValueSpec::string("hello "),
            string_source(&ctx, Slot::at(1)),
        ]);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect("write");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32, ValType::I32], Vec::new());
    }

    #[test]
    fn a_literal_joined_to_a_ref_in_locals_is_allocated_and_copied() {
        // A received param is in locals, the common case for joining.
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(2);
        let source = Slot::flat(vec![
            Local::new(0, ValType::I32),
            Local::new(1, ValType::I32),
        ]);
        let spec = ValueSpec::concat([ValueSpec::string("hello "), string_source(&ctx, source)]);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect("write");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32, ValType::I32], Vec::new());
    }

    #[test]
    fn a_joined_string_is_written_into_locals() {
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(2);
        let spec = ValueSpec::concat([
            ValueSpec::string("hello "),
            string_source(&ctx, Slot::at(1)),
        ]);
        let destination = Slot::flat(vec![
            Local::new(emitter.local(ValType::I32), ValType::I32),
            Local::new(emitter.local(ValType::I32), ValType::I32),
        ]);
        Writer::new(&ctx, &emitter)
            .write(ty, &destination, &spec)
            .expect("write");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32, ValType::I32], Vec::new());
    }

    #[test]
    fn each_part_is_copied_at_an_incrementing_offset() {
        // Three parts, so the cursor advances twice between the three copies.
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(2);
        let spec = ValueSpec::concat([
            ValueSpec::string("a"),
            string_source(&ctx, Slot::at(1)),
            ValueSpec::string("b"),
        ]);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect("write");
        let bytes = emitter.encode().expect("encode").into_raw_body();
        // `memory.copy` is 0xFC 0x0A: one per part.
        let copies = bytes
            .windows(2)
            .filter(|window| window == b"\xFC\x0A")
            .count();
        assert_eq!(copies, 3, "one copy per part: {bytes:02x?}");
    }

    #[test]
    fn a_joined_string_nests_in_a_record_field() {
        let ctx = context(
            r"package test:concatfield;
              interface i { record holder { greeting: string } f: func(h: holder); }
              world w { import i; }",
        );
        let ty = named_type(&ctx, "holder");
        let emitter = Emitter::new(2);
        let source = ValueRef {
            ty: wit_parser::Type::String,
            slot: Slot::at(1),
        };
        let spec = ValueSpec::record([(
            "greeting",
            ValueSpec::concat([ValueSpec::string("hello "), ValueSpec::source(source)]),
        )]);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect("write");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32, ValType::I32], Vec::new());
    }

    #[test]
    fn joining_rejects_a_part_that_is_not_a_byte_sequence() {
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(1);
        let spec = ValueSpec::concat([ValueSpec::string("n="), ValueSpec::u32(7)]);
        let error = Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect_err("a u32 part is neither a string nor a list<u8>");
        assert!(format!("{error:#}").contains("u32"), "{error:#}");
    }

    #[test]
    fn a_joined_string_cannot_be_written_to_a_non_string_position() {
        let ctx = context(
            r"package test:concatbadpos;
              interface i { type n = u32; f: func(x: n); }
              world w { import i; }",
        );
        let ty = named_type(&ctx, "n");
        let emitter = Emitter::new(2);
        let source = ValueRef {
            ty: wit_parser::Type::String,
            slot: Slot::at(1),
        };
        let spec = ValueSpec::concat([ValueSpec::string("a"), ValueSpec::source(source)]);
        let error = Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect_err("a u32 position takes neither a string nor a list<u8>");
        assert!(format!("{error:#}").contains("concat"), "{error:#}");
    }

    #[test]
    fn joining_nothing_is_accepted() {
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(1);
        Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::at(0),
                &ValueSpec::concat(Vec::<ValueSpec>::new()),
            )
            .expect("write");
        let function = emitter.encode().expect("encode");
        let body = function.clone().into_raw_body();
        // Nothing to sum and no cursor to advance means the length stays 0.
        assert!(!body.contains(&I32_ADD), "no arithmetic: {body:02x?}");
        validate_with_allocator(&ctx, function, vec![ValType::I32], Vec::new());
    }

    const BYTES_WIT: &str = r"package test:concatbytes;
        interface i { type blob = list<u8>; f: func(b: blob); }
        world w { import i; }";

    #[test]
    fn a_list_of_byte_literals_is_interned_as_a_single_unit() {
        // Interned like a string, so the body stores a `{ptr, len}` pair
        // rather than a per-element byte.
        let spec = ValueSpec::list([ValueSpec::u8(1), ValueSpec::u8(2)]);
        assert!(matches!(spec, ValueSpec::Leaf(Leaf::Bytes(ref b)) if b == &[1, 2]));
    }

    #[test]
    fn a_list_of_bytes_that_includes_any_value_ref_remains_a_list() {
        let source = ValueSpec::source(ValueRef {
            ty: wit_parser::Type::U8,
            slot: Slot::at(0),
        });
        let spec = ValueSpec::list([ValueSpec::u8(1), source]);
        assert!(matches!(spec, ValueSpec::List(_)));
    }

    #[test]
    fn byte_literals_are_written_to_a_list_of_u8() {
        writes_memory(BYTES_WIT, "blob", &ValueSpec::bytes(vec![1, 2, 3]));
    }

    #[test]
    fn a_byte_literal_joined_with_a_ref_is_allocated_and_copied() {
        let ctx = context(BYTES_WIT);
        let ty = named_type(&ctx, "blob");
        let emitter = Emitter::new(2);
        let spec = ValueSpec::concat([
            ValueSpec::bytes(vec![0xCA, 0xFE]),
            ValueSpec::source(ValueRef {
                ty,
                slot: Slot::at(1),
            }),
        ]);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect("write");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32, ValType::I32], Vec::new());
    }

    #[test]
    fn two_byte_value_refs_are_joined() {
        let ctx = context(BYTES_WIT);
        let ty = named_type(&ctx, "blob");
        let emitter = Emitter::new(3);
        let spec = ValueSpec::concat([
            ValueSpec::source(ValueRef {
                ty,
                slot: Slot::at(1),
            }),
            ValueSpec::source(ValueRef {
                ty,
                slot: Slot::at(2),
            }),
        ]);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect("write");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(
            &ctx,
            function,
            vec![ValType::I32, ValType::I32, ValType::I32],
            Vec::new(),
        );
    }

    #[test]
    fn joining_only_byte_literals_interns_one_entry() {
        let spec = ValueSpec::concat([ValueSpec::bytes(vec![1, 2]), ValueSpec::bytes(vec![3])]);
        assert!(matches!(spec, ValueSpec::Leaf(Leaf::Bytes(ref b)) if b == &[1, 2, 3]));
    }

    #[test]
    fn a_string_part_can_concat_with_bytes() {
        // A string and a list<u8> are both `{ptr, len}` over bytes, so a
        // string source contributes to a list<u8> destination.
        let ctx = context(
            r"package test:mixedbytes;
              interface i { type blob = list<u8>; type name = string; f: func(b: blob); }
              world w { import i; }",
        );
        let blob = named_type(&ctx, "blob");
        let emitter = Emitter::new(2);
        let spec = ValueSpec::concat([
            ValueSpec::bytes(vec![1]),
            ValueSpec::source(ValueRef {
                ty: named_type(&ctx, "name"),
                slot: Slot::at(1),
            }),
        ]);
        Writer::new(&ctx, &emitter)
            .write(blob, &Slot::at(0), &spec)
            .expect("a string source is bytes");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32, ValType::I32], Vec::new());
    }

    #[test]
    fn a_byte_value_ref_cannot_join_a_string() {
        // The reverse of the join above: a source's bytes might not be UTF-8.
        let ctx = context(
            r"package test:bytesintostring;
              interface i { type blob = list<u8>; type name = string; f: func(n: name); }
              world w { import i; }",
        );
        let name = named_type(&ctx, "name");
        let emitter = Emitter::new(2);
        let spec = ValueSpec::concat([
            ValueSpec::string("hello "),
            ValueSpec::source(ValueRef {
                ty: named_type(&ctx, "blob"),
                slot: Slot::at(1),
            }),
        ]);
        let error = Writer::new(&ctx, &emitter)
            .write(name, &Slot::at(0), &spec)
            .expect_err("a string destination rejects a byte value ref");
        assert!(format!("{error:#}").contains("UTF-8"), "{error:#}");
    }

    #[test]
    fn a_byte_literal_that_is_not_utf8_cannot_join_a_string() {
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(1);
        let spec = ValueSpec::concat([ValueSpec::string("hi"), ValueSpec::bytes(vec![0xFF])]);
        let error = Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect_err("a string destination rejects invalid UTF-8");
        assert!(format!("{error:#}").contains("UTF-8"), "{error:#}");
    }

    #[test]
    fn a_byte_literal_that_is_utf8_can_join_a_string() {
        let ctx = context(STRING_WIT);
        let ty = named_type(&ctx, "name");
        let emitter = Emitter::new(1);
        let spec = ValueSpec::concat([
            ValueSpec::string("hello "),
            ValueSpec::bytes(b"world".to_vec()),
        ]);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &spec)
            .expect("valid UTF-8 bytes join a string");
    }

    #[test]
    fn a_memory_source_is_copied_verbatim() {
        // A record has padding between its flats, so the copy must be
        // byte-for-byte rather than flat-by-flat.
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(2);
        let source = ValueRef {
            ty,
            slot: Slot::at(1),
        };
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &ValueSpec::source(source))
            .expect("copy");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32, ValType::I32], Vec::new());
    }

    #[test]
    fn a_flat_source_is_copied_local_by_local() {
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(1);
        let locals = vec![
            Local::new(emitter.local(ValType::I32), ValType::I32),
            Local::new(emitter.local(ValType::I64), ValType::I64),
        ];
        let source = ValueRef {
            ty,
            slot: Slot::flat(locals),
        };
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &ValueSpec::source(source))
            .expect("copy");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32], Vec::new());
    }

    #[test]
    fn a_source_cannot_overflow_its_flat_destination() {
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(1);
        let source = ValueRef {
            ty,
            slot: Slot::at(0),
        };
        // `point` flattens to two core values, but only one local is offered.
        let error = Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::flat(vec![Local::new(emitter.local(ValType::I32), ValType::I32)]),
                &ValueSpec::source(source),
            )
            .expect_err("a two-flat source cannot fit one local");
        assert!(format!("{error:#}").contains("only 1 locals"));
    }

    #[test]
    fn there_is_no_store_for_a_type_that_is_not_a_flat() {
        // A lossy fallback would have emitted an i32 store.
        assert!(Store::for_type(ValType::V128).is_err());
        assert!(Load::for_type(ValType::V128).is_err());
        assert!(flat_width(ValType::V128).is_err());
    }

    /// The raw body bytes of a write into memory.
    fn write_bytes(wit: &str, type_name: &str, value: &ValueSpec) -> Vec<u8> {
        let ctx = context(wit);
        let ty = named_type(&ctx, type_name);
        let emitter = Emitter::new(1);
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), value)
            .expect("write");
        emitter.encode().expect("encode").into_raw_body()
    }

    #[test]
    fn a_one_byte_discriminant_is_stored_at_one_byte() {
        // Two cases fit in a u8 tag. Storing it wider would overwrite the
        // padding before the payload and still type-check.
        let bytes = write_bytes(
            r"package test:discwidth;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
            "shape",
            &ValueSpec::variant_unit("empty"),
        );
        assert!(
            bytes.contains(&I32_STORE8),
            "the discriminant is stored as one byte"
        );
        assert!(
            !bytes.contains(&I32_STORE),
            "and not at full width: {bytes:02x?}"
        );
    }

    #[test]
    fn a_two_byte_discriminant_is_stored_at_two_bytes() {
        // 9 flags exceed a u8 repr, so the bitset is a u16.
        let bytes = write_bytes(
            r"package test:flagwidth;
              interface i {
                flags many { a, b, c, d, e, f, g, h, i }
                check: func(m: many);
              }
              world w { import i; }",
            "many",
            &ValueSpec::flags(["a", "i"]),
        );
        assert!(
            bytes.contains(&I32_STORE16),
            "a 9-flag bitset is stored as two bytes: {bytes:02x?}"
        );
    }

    #[test]
    fn a_variant_payload_is_written_past_its_discriminant() {
        // Flattened, a variant is [disc, ...payload]. If the payload slot
        // still included the discriminant local, the payload would overwrite
        // it, and the body would still validate.
        let ctx = context(
            r"package test:payloadslot;
              interface i { variant v { n(u32), empty } f: func(x: v); }
              world w { import i; }",
        );
        let ty = named_type(&ctx, "v");
        let emitter = Emitter::new(0);
        let locals: Vec<Local> = [ValType::I32, ValType::I32]
            .iter()
            .map(|core| Local::new(emitter.local(*core), *core))
            .collect();
        let disc_local = locals[0].index as u8;
        let payload_local = locals[1].index as u8;
        Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::flat(locals.clone()),
                &ValueSpec::variant("n", ValueSpec::u32(7)),
            )
            .expect("write");
        let bytes = emitter.encode().expect("encode").into_raw_body();
        // local.set is 0x21; the payload must land in the second local.
        let sets: Vec<u8> = bytes
            .windows(2)
            .filter(|window| window[0] == 0x21)
            .map(|window| window[1])
            .collect();
        assert!(
            sets.contains(&disc_local) && sets.contains(&payload_local),
            "the discriminant and the payload occupy different locals: {sets:?}"
        );
    }

    #[test]
    fn a_flat_composite_gives_each_member_its_own_sub_range() {
        // A record written into locals: each field takes a distinct sub-range,
        // so the two fields must land in two different locals. Collapsing the
        // ranges would write both to the first and still validate.
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(0);
        let locals: Vec<Local> = [ValType::I32, ValType::I64]
            .iter()
            .map(|core| Local::new(emitter.local(*core), *core))
            .collect();
        let indices: Vec<u8> = locals.iter().map(|local| local.index as u8).collect();
        Writer::new(&ctx, &emitter)
            .write(
                ty,
                &Slot::flat(locals.clone()),
                &ValueSpec::record([("x", ValueSpec::u32(1)), ("y", ValueSpec::u64(2))]),
            )
            .expect("write");
        let bytes = emitter.encode().expect("encode").into_raw_body();
        let sets: Vec<u8> = bytes
            .windows(2)
            .filter(|window| window[0] == 0x21)
            .map(|window| window[1])
            .collect();
        for index in &indices {
            assert!(
                sets.contains(index),
                "field written to local {index}: sets were {sets:?}"
            );
        }
    }

    #[test]
    fn a_length_is_stored_after_its_pointer() {
        // {ptr, len} is two i32s; writing the length over the pointer would
        // leave a valid body pointing at the wrong address.
        let bytes = write_bytes(
            r"package test:ptrlen;
              interface i { type bytes = list<u8>; f: func(b: bytes); }
              world w { import i; }",
            "bytes",
            &ValueSpec::list([ValueSpec::u8(1), ValueSpec::u8(2)]),
        );
        // Two i32 stores write the pair: offsets 0 and 4.
        let offsets: Vec<u8> = bytes
            .windows(3)
            .filter(|window| window[0] == I32_STORE)
            .map(|window| window[2])
            .collect();
        assert!(
            offsets.contains(&0) && offsets.contains(&4),
            "the pointer is at 0 and the length at 4: {offsets:?}"
        );
    }

    #[test]
    fn record_fields_are_written_at_their_declared_offsets() {
        // `point` is { x: u32, y: u64 }: the u64 is 8-aligned, so y sits at 8,
        // not 4. Writing both at 0 would overwrite x and still validate.
        let bytes = write_bytes(
            RECORD_WIT,
            "point",
            &ValueSpec::record([("x", ValueSpec::u32(1)), ("y", ValueSpec::u64(2))]),
        );
        let x_offset = bytes
            .windows(3)
            .find(|window| window[0] == I32_STORE)
            .map(|window| window[2])
            .expect("the u32 field is stored");
        let y_offset = bytes
            .windows(3)
            .find(|window| window[0] == I64_STORE)
            .map(|window| window[2])
            .expect("the u64 field is stored");
        assert_eq!(x_offset, 0, "x is at the start");
        assert_eq!(y_offset, 8, "y follows x's alignment padding");
    }

    #[test]
    fn a_memory_source_copies_the_whole_type() {
        // The copy length is the type's size, so a padded record moves intact.
        // A fixed length would truncate it and still validate.
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(2);
        let source = ValueRef {
            ty,
            slot: Slot::at(1),
        };
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::at(0), &ValueSpec::source(source))
            .expect("copy");
        let bytes = emitter.encode().expect("encode").into_raw_body();
        let size = ctx.layout().size(&ty) as u8;
        assert_eq!(size, 16, "u32 + padding + u64");
        // The i32.const carrying the length precedes memory.copy (0xFC 0x0A).
        let copy = bytes
            .windows(2)
            .position(|window| window == [0xFC, 0x0A])
            .expect("a memory.copy is emitted");
        assert_eq!(
            bytes[copy - 1],
            size,
            "the copy length is the type's size: {bytes:02x?}"
        );
    }

    #[test]
    fn a_source_exactly_filling_its_destination_is_accepted() {
        // The boundary: two flats into two locals is legal, so the check is
        // `>`, not `>=`.
        let ctx = context(RECORD_WIT);
        let ty = named_type(&ctx, "point");
        let emitter = Emitter::new(1);
        let source = ValueRef {
            ty,
            slot: Slot::at(0),
        };
        let locals = vec![
            Local::new(emitter.local(ValType::I32), ValType::I32),
            Local::new(emitter.local(ValType::I64), ValType::I64),
        ];
        Writer::new(&ctx, &emitter)
            .write(ty, &Slot::flat(locals), &ValueSpec::source(source))
            .expect("a source may exactly fill its destination");
    }

    #[test]
    fn a_discriminant_narrows_to_its_tag_width() {
        assert!(matches!(Store::for_tag(Int::U8), Store::I32To8));
        assert!(matches!(Store::for_tag(Int::U16), Store::I32To16));
        assert!(matches!(Store::for_tag(Int::U32), Store::I32));
        assert!(matches!(Store::for_tag(Int::U64), Store::I64));
        // A narrowing store still consumes an i32.
        assert_eq!(Store::for_tag(Int::U8).operand(), ValType::I32);
        assert_eq!(Store::for_tag(Int::U64).operand(), ValType::I64);
    }
}
