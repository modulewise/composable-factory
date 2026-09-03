//! The world a component implements, as a factory navigates it: its types, its
//! imported and exported functions, and the values at their positions.
//! Everything here describes and/or points at something in the resolved world.

use std::cell::Cell;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail};
use wasm_encoder::{BlockType, Instruction, ValType};
use wit_parser::{Int, Resolve, TypeDefKind, WorldItem, WorldKey};

use crate::abi;
use crate::emitter::Emitter;
use crate::values::{
    BuildContext, Len, Load, Loader, Local, Size, Slot, ValueRef, Writer, call_allocator,
    load_ptr_len, member_slots, reserve,
};

pub use crate::values::ValueSpec;

/// A variant-like type's cases in discriminant order: each name is paired with
/// its payload type, or `None` for a payload-less case.
type Cases = Vec<(String, Option<wit_parser::Type>)>;

/// A type at some position in the component's WIT. Reached from a function's
/// params or result, and from there by descending through [`Kind`].
#[derive(Clone)]
pub struct Type {
    ctx: Rc<BuildContext>,
    wit: wit_parser::Type,
}

impl Type {
    pub(crate) fn new(ctx: Rc<BuildContext>, wit: wit_parser::Type) -> Self {
        Type { ctx, wit }
    }

    /// This type's declared name, if it has one. Primitives and inline
    /// composites do not.
    pub fn name(&self) -> Option<&str> {
        match self.wit {
            wit_parser::Type::Id(id) => self.ctx.resolve().types[id].name.as_deref(),
            _ => None,
        }
    }

    /// What this type is, with its children as [`Type`]s. Aliases are
    /// followed, so a `use`d type reports what it refers to.
    pub fn kind(&self) -> Kind {
        let id = match self.wit {
            wit_parser::Type::Bool => return Kind::Bool,
            wit_parser::Type::U8 => return Kind::U8,
            wit_parser::Type::U16 => return Kind::U16,
            wit_parser::Type::U32 => return Kind::U32,
            wit_parser::Type::U64 => return Kind::U64,
            wit_parser::Type::S8 => return Kind::S8,
            wit_parser::Type::S16 => return Kind::S16,
            wit_parser::Type::S32 => return Kind::S32,
            wit_parser::Type::S64 => return Kind::S64,
            wit_parser::Type::F32 => return Kind::F32,
            wit_parser::Type::F64 => return Kind::F64,
            wit_parser::Type::Char => return Kind::Char,
            wit_parser::Type::String => return Kind::String,
            wit_parser::Type::ErrorContext => return Kind::ErrorContext,
            wit_parser::Type::Id(id) => id,
        };
        match &self.ctx.resolve().types[id].kind {
            TypeDefKind::Type(inner) => self.child(*inner).kind(),
            TypeDefKind::Record(record) => Kind::Record(
                record
                    .fields
                    .iter()
                    .map(|field| Field {
                        name: field.name.clone(),
                        ty: self.child(field.ty),
                    })
                    .collect(),
            ),
            TypeDefKind::Variant(variant) => Kind::Variant(
                variant
                    .cases
                    .iter()
                    .map(|case| Case {
                        name: case.name.clone(),
                        payload: case.ty.map(|ty| self.child(ty)),
                    })
                    .collect(),
            ),
            TypeDefKind::Enum(e) => Kind::Enum(e.cases.iter().map(|c| c.name.clone()).collect()),
            TypeDefKind::Flags(f) => Kind::Flags(f.flags.iter().map(|f| f.name.clone()).collect()),
            TypeDefKind::Option(ty) => Kind::Option(self.child(*ty)),
            TypeDefKind::Result(result) => Kind::Result {
                ok: result.ok.map(|ty| self.child(ty)),
                err: result.err.map(|ty| self.child(ty)),
            },
            TypeDefKind::List(ty) => Kind::List(self.child(*ty)),
            TypeDefKind::FixedLengthList(ty, len) => Kind::FixedLengthList(self.child(*ty), *len),
            TypeDefKind::Tuple(tuple) => {
                Kind::Tuple(tuple.types.iter().map(|ty| self.child(*ty)).collect())
            }
            TypeDefKind::Map(key, value) => Kind::Map(self.child(*key), self.child(*value)),
            TypeDefKind::Future(ty) => Kind::Future(ty.map(|ty| self.child(ty))),
            TypeDefKind::Stream(ty) => Kind::Stream(ty.map(|ty| self.child(ty))),
            TypeDefKind::Handle(handle) => {
                let resource = match handle {
                    wit_parser::Handle::Own(id) | wit_parser::Handle::Borrow(id) => *id,
                };
                Kind::Handle(self.child(wit_parser::Type::Id(resource)))
            }
            TypeDefKind::Resource => Kind::Resource,
            // A placeholder wit-parser leaves for a type it could not resolve,
            // which a parsed world never contains.
            TypeDefKind::Unknown => unreachable!("unresolved type in a parsed world"),
        }
    }

    /// The raw type, for the layers that address it directly.
    pub(crate) fn wit(&self) -> wit_parser::Type {
        self.wit
    }

    /// Another type in the same component.
    fn child(&self, wit: wit_parser::Type) -> Type {
        Type {
            ctx: Rc::clone(&self.ctx),
            wit,
        }
    }

    /// This variant-like type decomposed: its cases in discriminant order, the
    /// tag width, and the byte offset every payload sits at.
    ///
    /// The shared decomposition behind every walk over a tagged type, so
    /// `variant`, `enum`, `option` and `result` are stated in one place rather
    /// than re-derived per walk.
    fn variant_cases(&self) -> Result<(Cases, Int, usize)> {
        let resolve = self.ctx.resolve();
        // Follow aliases to the underlying definition.
        let mut wit = self.wit;
        let id = loop {
            let wit_parser::Type::Id(id) = wit else {
                bail!("not a variant-like type");
            };
            match &resolve.types[id].kind {
                TypeDefKind::Type(inner) => wit = *inner,
                _ => break id,
            }
        };
        let (cases, tag): (Cases, Int) = match &resolve.types[id].kind {
            TypeDefKind::Variant(variant) => (
                variant
                    .cases
                    .iter()
                    .map(|case| (case.name.clone(), case.ty))
                    .collect(),
                variant.tag(),
            ),
            TypeDefKind::Enum(declared) => (
                declared
                    .cases
                    .iter()
                    .map(|case| (case.name.clone(), None))
                    .collect(),
                declared.tag(),
            ),
            TypeDefKind::Option(inner) => (
                vec![
                    ("none".to_string(), None),
                    ("some".to_string(), Some(*inner)),
                ],
                Int::U8,
            ),
            TypeDefKind::Result(result) => (
                vec![
                    ("ok".to_string(), result.ok),
                    ("err".to_string(), result.err),
                ],
                Int::U8,
            ),
            other => bail!("unsupported type kind {other:?} (not variant-like)"),
        };
        let payload_offset = self
            .ctx
            .layout()
            .payload_offset(tag, cases.iter().map(|(_, ty)| ty.as_ref()));
        Ok((cases, tag, payload_offset))
    }
}

/// A value at some position in a function body. Stored as bytes in memory or
/// core values in locals; so it can be read, written, and handed to a call.
#[derive(Clone)]
pub struct Value {
    ty: Type,
    slot: Slot,
    emitter: Emitter,
    /// Whether this value has been written. Shared across clones, since the
    /// implementor writes one clone while the builder reads another.
    written: Rc<Cell<bool>>,
}

/// The planned/built boundary: a [`ValueSpec`] is a recipe for building a
/// [`Value`]. This lets a planned composite carry an already-built member by
/// handing the spec a source it can ask to emit itself.
impl From<&Value> for ValueSpec {
    fn from(value: &Value) -> Self {
        ValueSpec::source(value.as_ref())
    }
}

impl From<Value> for ValueSpec {
    fn from(value: Value) -> Self {
        ValueSpec::source(value.as_ref())
    }
}

impl Value {
    /// Pair a type with the slot its bytes occupy.
    pub(crate) fn new(ty: Type, slot: Slot, emitter: Emitter) -> Self {
        Value {
            ty,
            slot,
            emitter,
            written: Rc::new(Cell::new(false)),
        }
    }

    /// This value's type, as a navigable node.
    pub fn ty(&self) -> Type {
        self.ty.clone()
    }

    /// The base-pointer local, for a value held in memory.
    pub(crate) fn base(&self) -> Option<u32> {
        self.slot.base()
    }

    /// Whether this value has been written. Checked when deciding to deliver.
    pub(crate) fn was_written(&self) -> bool {
        self.written.get()
    }

    /// This value's representation for the layer below: raw type and slot.
    fn as_ref(&self) -> ValueRef {
        ValueRef {
            ty: self.ty.wit(),
            slot: self.slot.clone(),
        }
    }

    /// A list or map element as a value addressed from `base`, since elements
    /// live in their own allocation rather than at an offset from this value.
    /// For a map, `offset` points to the key or value within an entry.
    fn at_base(&self, ty: Type, base: u32, offset: usize) -> Value {
        Value::new(ty, Slot::Memory { base, offset }, self.emitter.clone())
    }

    fn emit(&self, instruction: Instruction<'static>) {
        self.emitter.emit(instruction);
    }

    fn local(&self, ty: ValType) -> u32 {
        self.emitter.local(ty)
    }

    fn br(&self, label: &str) -> Result<()> {
        self.emitter.br(label)
    }

    fn br_if(&self, label: &str) -> Result<()> {
        self.emitter.br_if(label)
    }

    fn block(&self, label: &str, body: impl FnOnce() -> Result<()>) -> Result<()> {
        self.emitter.block(label, BlockType::Empty, body)
    }

    fn loop_(&self, label: &str, body: impl FnOnce() -> Result<()>) -> Result<()> {
        self.emitter.loop_(label, BlockType::Empty, body)
    }

    /// A writer over this value's context.
    fn writer(&self) -> Writer<'_> {
        Writer::new(&self.ty.ctx, &self.emitter)
    }

    /// A local holding `base + offset`, or `base` itself when the offset is
    /// zero. A layout concern, though it composes from raw emitter ops.
    fn pointer_at(&self, base: u32, offset: usize) -> u32 {
        if offset == 0 {
            return base;
        }
        let pointer = self.local(ValType::I32);
        self.emit(Instruction::LocalGet(base));
        self.emit(Instruction::I32Const(offset as i32));
        self.emit(Instruction::I32Add);
        self.emit(Instruction::LocalSet(pointer));
        pointer
    }

    /// Leave this value's flats on the stack in ABI order, ready for a call or
    /// a return to consume. The caller doesn't need to know the slot type.
    ///
    /// A visitor's callbacks run inside empty blocks, so nothing may be left
    /// on the stack when one ends.
    pub fn push(&self) -> Result<()> {
        match &self.slot {
            Slot::Memory { base, offset } => {
                // The loader addresses from a bare base so include any offset.
                let base = self.pointer_at(*base, *offset);
                Loader::new(&self.ty.ctx, &self.emitter).load(self.ty.wit(), base)?;
            }
            Slot::Flat { locals } => {
                for local in locals {
                    self.emit(Instruction::LocalGet(local.index));
                }
            }
        }
        Ok(())
    }

    /// Write this variant-like value's discriminant and hand back the slot its
    /// payload occupies.
    fn write_disc(&self, tag: Int, disc: i64, payload_offset: usize) -> Result<Slot> {
        self.writer()
            .write_disc(&self.slot, tag, disc, payload_offset)
    }

    /// Write `spec` into this value.
    pub fn write(&self, spec: &ValueSpec) -> Result<()> {
        self.writer()
            .write(self.ty.wit(), &self.slot, spec)
            // Leads the chain, so path frames below read as a continuation.
            .with_context(|| match self.ty.name() {
                Some(name) => format!("failed to write a value of type `{name}`"),
                None => "failed to write a value".to_string(),
            })?;
        self.written.set(true);
        Ok(())
    }

    /// Convert this scalar to `target`, including signedness if relevant.
    pub fn coerce(&self, target: impl Into<wit_parser::Type>) -> Result<Value> {
        let target = target.into();
        let resolve = self.ty.ctx.resolve();
        let from = self.ty.wit();
        let from_flats = abi::flat_types(resolve, from)?;
        let to_flats = abi::flat_types(resolve, target)?;
        if from_flats.len() > 1 || to_flats.len() > 1 {
            bail!(
                "coerce: single-flat scalars only; {from:?} -> {target:?} spans more than one \
                 core value (build a composite with `write`)"
            );
        }
        let to = self.ty.child(target);
        // Same core representation (`s32` to `u32`, `char` to `u32`, an alias)
        // means nothing to emit, so the value is retyped in its existing slot.
        let Some(instruction) = coercion(&self.ty.kind(), &to.kind())? else {
            if from_flats != to_flats {
                bail!("coerce: conversion from {from:?} to {target:?} is not supported");
            }
            return Ok(Value::new(to, self.slot.clone(), self.emitter.clone()));
        };
        let core = to_flats.first().copied().unwrap_or(ValType::I32);
        self.push()?;
        self.emit(instruction);
        let local = Local::new(self.local(core), core);
        self.emit(Instruction::LocalSet(local.index));
        Ok(Value::new(
            to,
            Slot::flat(vec![local]),
            self.emitter.clone(),
        ))
    }

    /// Branch on this variant-like value's case, running each arm's body
    /// inside its own branch with that case's payload. The payload is only
    /// valid inside its arm, whose locals hold it there. If code after the
    /// dispatch expects to read something a branch produced, every producer
    /// must write it to the same place, established before the dispatch.
    /// A case with no arm emits nothing.
    pub fn dispatch(&self, arms: Vec<MatchArm<'_>>) -> Result<()> {
        let (cases, _, payload_offset) = self.ty.variant_cases()?;
        let disc = self.load_discriminant()?;
        // Resolve each arm to its case index and payload type, which validates
        // the case name before anything is emitted.
        let mut resolved = Vec::with_capacity(arms.len());
        for arm in arms {
            let (index, payload) = cases
                .iter()
                .enumerate()
                .find(|(_, (name, _))| name == &arm.case)
                .map(|(index, (_, ty))| (index, *ty))
                .ok_or_else(|| anyhow!("dispatch: no case '{}' in this value's type", arm.case))?;
            resolved.push((index, payload, arm));
        }
        self.dispatch_arms(disc, payload_offset, &mut resolved.into_iter())
    }

    /// The if/else block over the arms a caller supplied.
    fn dispatch_arms(
        &self,
        disc: u32,
        payload_offset: usize,
        arms: &mut dyn Iterator<Item = (usize, Option<wit_parser::Type>, MatchArm<'_>)>,
    ) -> Result<()> {
        let Some((case, payload, arm)) = arms.next() else {
            return Ok(());
        };
        self.emit(Instruction::LocalGet(disc));
        self.emit(Instruction::I32Const(case as i32));
        self.emit(Instruction::I32Eq);
        self.emitter
            .if_(BlockType::Empty, || {
                let payload = payload.map(|ty| {
                    Value::new(
                        self.ty.child(ty),
                        self.payload_slot(payload_offset),
                        self.emitter.clone(),
                    )
                });
                (arm.body)(payload)
            })?
            .else_(|| self.dispatch_arms(disc, payload_offset, arms))
    }

    /// Assert this value carries `case` and return its payload, trapping at
    /// runtime if it does not. For a case with no payload, there is no value
    /// to return; use [`Value::dispatch`] to branch instead.
    pub fn assert_case(&self, case: &str) -> Result<Value> {
        let (cases, _, payload_offset) = self.ty.variant_cases()?;
        let (index, payload) = cases
            .iter()
            .enumerate()
            .find(|(_, (name, _))| name == case)
            .map(|(index, (_, ty))| (index, *ty))
            .ok_or_else(|| anyhow!("assert_case: no case '{case}' in this value's type"))?;
        let payload = payload.ok_or_else(|| {
            anyhow!(
                "assert_case: case '{case}' carries no payload, so there is nothing to return; \
                 use `dispatch` to branch on a payload-less case"
            )
        })?;
        let disc = self.load_discriminant()?;
        self.emit(Instruction::LocalGet(disc));
        self.emit(Instruction::I32Const(index as i32));
        self.emit(Instruction::I32Ne);
        self.emitter.if_(BlockType::Empty, || {
            self.emitter.trap();
            Ok(())
        })?;
        Ok(Value::new(
            self.ty.child(payload),
            self.payload_slot(payload_offset),
            self.emitter.clone(),
        ))
    }

    /// Where this variant-like value's payload lives, the read-side twin of
    /// [`Value::write_disc`]'s return.
    ///
    /// The payload type varies per case, so this is the slot only: each arm
    /// pairs it with its own case's type.
    fn payload_slot(&self, payload_offset: usize) -> Slot {
        match &self.slot {
            Slot::Flat { locals } => Slot::flat(locals[1..].to_vec()),
            Slot::Memory { base, offset } => Slot::Memory {
                base: *base,
                offset: offset + payload_offset,
            },
        }
    }

    /// Load this variant-like value's discriminant into a local of its own,
    /// widening the tag's declared width to i32.
    ///
    /// Always a fresh local. The discriminant stays live across a whole
    /// [`Value::dispatch`] chain, and an arm may write into the dispatched
    /// value, so a copy ensures that each arm tests the original case.
    fn load_discriminant(&self) -> Result<u32> {
        let (_, tag, _) = self.ty.variant_cases()?;
        let disc = self.local(ValType::I32);
        match &self.slot {
            Slot::Memory { base, offset } => {
                self.emit(Instruction::LocalGet(*base));
                self.emit(Load::for_tag(tag).instruction(*offset));
            }
            Slot::Flat { locals } => {
                let Some(local) = locals.first() else {
                    bail!("a variant needs at least a discriminant local");
                };
                self.emit(Instruction::LocalGet(local.index));
            }
        }
        self.emit(Instruction::LocalSet(disc));
        Ok(disc)
    }

    /// Release this resource handle, emitting the resource type's
    /// `resource.drop` builtin.
    ///
    /// A non-handle is an error, not a no-op: silently emitting nothing would
    /// leak the handle that was meant to be released.
    pub fn drop(&self) -> Result<()> {
        let resource = handle_resource(self.ty.ctx.resolve(), self.ty.wit()).ok_or_else(|| {
            anyhow!(
                "drop: this value is a {}, not a resource handle",
                self.ty.kind().name()
            )
        })?;
        let index = abi::drop_index(self.ty.ctx.imports(), resource)
            .ok_or_else(|| anyhow!("drop: this resource is not imported"))?;
        self.push()?;
        self.emit(Instruction::Call(index));
        Ok(())
    }

    /// Traverse this value, driving `visitor` per node.
    ///
    /// Returns nothing: whatever the visitor accumulates is collected from the
    /// visitor itself.
    pub fn read_with(&self, visitor: &mut dyn ReadVisitor) -> Result<()> {
        self.read_walk(visitor)
    }

    /// Build this value's content by walking its type and asking `visitor`
    /// what to write at each leaf.
    ///
    /// The walk handles member offsets, element loops, and variant dispatches.
    /// The visitor supplies a spec per leaf node, and the walk writes into
    /// each corresponding slot, so nothing is returned. The value it was
    /// called on is the result.
    pub fn write_with(&self, visitor: &mut dyn WriteVisitor) -> Result<()> {
        self.write_walk(visitor)?;
        // A composite descends to member values, so leaf writes land on those.
        // What matters is that this value was the destination.
        self.written.set(true);
        Ok(())
    }

    /// Report each node of this value to `visitor`.
    ///
    /// Shape comes from [`Kind`], layout from the
    /// [`Layout`](crate::abi::Layout). A composite descends by producing a
    /// member `Value` and walking that.
    fn read_walk(&self, visitor: &mut dyn ReadVisitor) -> Result<()> {
        match self.ty.kind() {
            Kind::Record(fields) => {
                let types: Vec<wit_parser::Type> =
                    fields.iter().map(|field| field.ty().wit()).collect();
                let slots = member_slots(&self.ty.ctx, &self.slot, &types)?;
                visitor.begin_record()?;
                for (field, slot) in fields.iter().zip(slots) {
                    visitor.begin_field(field.name())?;
                    self.at_slot(field.ty(), slot).read_walk(visitor)?;
                    visitor.end_field()?;
                }
                visitor.end_record()?;
            }
            Kind::Tuple(members) => {
                // A tuple is a positional record: same member math, no names.
                let types: Vec<wit_parser::Type> = members.iter().map(|ty| ty.wit()).collect();
                let slots = member_slots(&self.ty.ctx, &self.slot, &types)?;
                visitor.begin_tuple()?;
                for (ty, slot) in members.iter().zip(slots) {
                    self.at_slot(ty.clone(), slot).read_walk(visitor)?;
                }
                visitor.end_tuple()?;
            }
            Kind::List(elem) => {
                // `list<T>` is `{ptr, len}`; the elements need a loop.
                visitor.begin_list()?;
                self.read_elements(elem, visitor)?;
                visitor.end_list()?;
            }
            Kind::FixedLengthList(elem, count) => {
                // N contiguous elements inline, so the length is known while
                // emitting and no runtime loop is needed. Bracketed by
                // `begin_list`/`end_list` so a visitor treats it consistently
                // with the dynamic case.
                let types = vec![elem.wit(); count as usize];
                let slots = member_slots(&self.ty.ctx, &self.slot, &types)?;
                visitor.begin_list()?;
                for slot in slots {
                    self.at_slot(elem.clone(), slot).read_walk(visitor)?;
                }
                visitor.end_list()?;
            }
            Kind::Map(key, value) => {
                // A map flattens to `{ptr, len}` like a list, with each entry
                // laid out as a 2-member record. The entry has no type of its
                // own to descend into, so the key and value are walked directly.
                visitor.begin_map()?;
                self.read_entries([key, value], visitor)?;
                visitor.end_map()?;
            }
            Kind::Flags(names) => {
                visitor.begin_flags()?;
                self.read_flags(&names, visitor)?;
                visitor.end_flags()?;
            }
            kind if kind.is_variant_like() => {
                self.read_variant(visitor)?;
            }
            leaf => {
                // Every composite is handled above, so anything here
                // terminates the walk. A newly added composite kind would trip
                // this instead of being misreported to the visitor as a leaf.
                debug_assert!(
                    !leaf.is_composite(),
                    "composite kind `{}` reached the leaf arm",
                    leaf.name()
                );
                // A leaf in memory is reported at its own base, so the visitor
                // receives a self-contained value rather than one addressed
                // relative to its parent.
                let value = match &self.slot {
                    Slot::Memory { base, offset } => {
                        let base = self.pointer_at(*base, *offset);
                        Value::new(self.ty(), Slot::at(base), self.emitter.clone())
                    }
                    Slot::Flat { .. } => self.clone(),
                };
                dispatch_leaf(leaf, value, visitor)?;
            }
        }
        Ok(())
    }

    /// A member of this value at an already-computed slot.
    fn at_slot(&self, ty: Type, slot: Slot) -> Value {
        Value::new(ty, slot, self.emitter.clone())
    }

    /// The runtime element loop shared by `list<T>` and `map<K,V>`: the value
    /// is `{pointer, length}` and element `i` sits at `pointer + i * stride`.
    fn read_elements(&self, elem: Type, visitor: &mut dyn ReadVisitor) -> Result<()> {
        let stride = self.ty.ctx.layout().size(&elem.wit());
        let (pointer, length) = self.load_ptr_len()?;
        let index = self.local(ValType::I32);
        let element = self.local(ValType::I32);
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(index));
        self.block("element_done", || {
            self.loop_("element_loop", || {
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::LocalGet(length));
                self.emit(Instruction::I32GeU);
                self.br_if("element_done")?;
                self.emit(Instruction::LocalGet(pointer));
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::I32Const(stride as i32));
                self.emit(Instruction::I32Mul);
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalSet(element));
                // Elements live in the heap allocation at the pointer, whether
                // the list handle itself is flat or in memory, so each gets
                // its own base rather than an offset from this value's.
                self.at_base(elem.clone(), element, 0).read_walk(visitor)?;
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::I32Const(1));
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalSet(index));
                self.br("element_loop")
            })
        })
    }

    /// The element loop for a `map<K,V>`, whose entries are `(K, V)` pairs
    /// laid out like a two-field tuple. Each entry brackets as a tuple, so a
    /// visitor sees the key and value as tuple elements.
    fn read_entries(&self, pair: [Type; 2], visitor: &mut dyn ReadVisitor) -> Result<()> {
        let types = [pair[0].wit(), pair[1].wit()];
        let offsets = self.ty.ctx.layout().field_offsets(types.iter());
        let stride = self.ty.ctx.layout().record_size(types.iter());
        let (pointer, length) = self.load_ptr_len()?;
        let index = self.local(ValType::I32);
        let entry = self.local(ValType::I32);
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(index));
        self.block("entry_done", || {
            self.loop_("entry_loop", || {
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::LocalGet(length));
                self.emit(Instruction::I32GeU);
                self.br_if("entry_done")?;
                self.emit(Instruction::LocalGet(pointer));
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::I32Const(stride as i32));
                self.emit(Instruction::I32Mul);
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalSet(entry));
                visitor.begin_tuple()?;
                for (ty, offset) in pair.iter().zip(&offsets) {
                    self.at_base(ty.clone(), entry, *offset)
                        .read_walk(visitor)?;
                }
                visitor.end_tuple()?;
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::I32Const(1));
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalSet(index));
                self.br("entry_loop")
            })
        })
    }

    /// Load this value's `{ptr, len}` pair and return the locals holding them.
    fn load_ptr_len(&self) -> Result<(u32, u32)> {
        load_ptr_len(&self.emitter, &self.slot)
    }

    /// Walk a flags bitset: test each declared flag's bit and report it within
    /// that test. Any subset may be set. Flag `i` is bit `i % 32` of word
    /// `i / 32`.
    fn read_flags(&self, names: &[String], visitor: &mut dyn ReadVisitor) -> Result<()> {
        let count = names.len();
        for (index, name) in names.iter().enumerate() {
            let word = index / 32;
            let bit = index % 32;
            match &self.slot {
                Slot::Memory { base, offset } => {
                    let (load, at) = if count <= 8 {
                        (Load::I32From8, *offset)
                    } else if count <= 16 {
                        (Load::I32From16, *offset)
                    } else {
                        (Load::I32, offset + word * 4)
                    };
                    self.emit(Instruction::LocalGet(*base));
                    self.emit(load.instruction(at));
                }
                Slot::Flat { locals } => {
                    let Some(local) = locals.get(word) else {
                        bail!("flags need {} local(s), got {}", word + 1, locals.len());
                    };
                    self.emit(Instruction::LocalGet(local.index));
                }
            }
            // if (word >> bit) & 1 { on_flag(name) }
            self.emit(Instruction::I32Const(bit as i32));
            self.emit(Instruction::I32ShrU);
            self.emit(Instruction::I32Const(1));
            self.emit(Instruction::I32And);
            self.emitter
                .if_(BlockType::Empty, || visitor.on_flag(name))?;
        }
        Ok(())
    }

    /// Walk a variant-like node: load the discriminant, then emit one branch
    /// per case, invoking the visitor inside each with that case's payload.
    fn read_variant(&self, visitor: &mut dyn ReadVisitor) -> Result<()> {
        let (cases, _, payload_offset) = self.ty.variant_cases()?;
        let disc = self.load_discriminant()?;
        let payload_slot = self.payload_slot(payload_offset);
        visitor.begin_variant()?;
        self.read_cases(disc, &payload_slot, &cases, 0, visitor)?;
        visitor.end_variant()?;
        Ok(())
    }

    /// The if/else chain over a variant's cases, recursing the payload walk
    /// within each arm.
    fn read_cases(
        &self,
        disc: u32,
        payload_slot: &Slot,
        cases: &[(String, Option<wit_parser::Type>)],
        case: usize,
        visitor: &mut dyn ReadVisitor,
    ) -> Result<()> {
        let Some((name, payload)) = cases.get(case) else {
            return Ok(());
        };
        let (name, payload) = (name.clone(), *payload);
        let arm = |visitor: &mut dyn ReadVisitor| -> Result<()> {
            visitor.begin_case(&name, payload.is_some())?;
            if let Some(payload) = payload {
                self.at_slot(self.ty.child(payload), payload_slot.clone())
                    .read_walk(visitor)?;
            }
            visitor.end_case()
        };
        // The last case needs no test. Every other case has been ruled out.
        if case + 1 == cases.len() {
            return arm(visitor);
        }
        // An n-way chain: the else arm recurses into the next case. `if_`
        // takes the then arm and `else_` the recursion, so the same
        // `&mut dyn ReadVisitor` is borrowed by each in turn.
        self.emit(Instruction::LocalGet(disc));
        self.emit(Instruction::I32Const(case as i32));
        self.emit(Instruction::I32Eq);
        self.emitter
            .if_(BlockType::Empty, || arm(visitor))?
            .else_(|| self.read_cases(disc, payload_slot, cases, case + 1, visitor))
    }

    /// Walk this value's type, asking `visitor` for the content at each node.
    /// The write-side mirror of [`Value::read_walk`].
    fn write_walk(&self, visitor: &mut dyn WriteVisitor) -> Result<()> {
        match self.ty.kind() {
            Kind::Record(fields) => {
                let types: Vec<wit_parser::Type> =
                    fields.iter().map(|field| field.ty().wit()).collect();
                let slots = member_slots(&self.ty.ctx, &self.slot, &types)?;
                for (field, slot) in fields.iter().zip(slots) {
                    visitor.begin_field(field.name())?;
                    self.at_slot(field.ty(), slot).write_walk(visitor)?;
                    visitor.end_field()?;
                }
                Ok(())
            }
            Kind::Tuple(members) => {
                // A tuple is a positional record: same member math, an index
                // instead of a name.
                let types: Vec<wit_parser::Type> = members.iter().map(|ty| ty.wit()).collect();
                let slots = member_slots(&self.ty.ctx, &self.slot, &types)?;
                for (index, (ty, slot)) in members.iter().zip(slots).enumerate() {
                    let position = self.index_value(index)?;
                    visitor.begin_element(&position)?;
                    self.at_slot(ty.clone(), slot).write_walk(visitor)?;
                    visitor.end_element()?;
                }
                Ok(())
            }
            Kind::FixedLengthList(elem, count) => {
                // The length is known while emitting, so the elements are
                // written directly into this value: no runtime loop, no
                // `length` query, no separate allocation.
                let types = vec![elem.wit(); count as usize];
                let slots = member_slots(&self.ty.ctx, &self.slot, &types)?;
                for (index, slot) in slots.into_iter().enumerate() {
                    let position = self.index_value(index)?;
                    visitor.begin_element(&position)?;
                    self.at_slot(elem.clone(), slot).write_walk(visitor)?;
                    visitor.end_element()?;
                }
                Ok(())
            }
            kind if kind.is_variant_like() => self.write_variant(visitor),
            Kind::List(elem) => self.write_elements(&[elem.wit()], visitor),
            Kind::Map(key, value) => {
                // A map flattens to `{ptr, len}` like a list, so each entry is
                // laid out as a 2-member record. The element loop is shared.
                self.write_elements(&[key.wit(), value.wit()], visitor)
            }
            Kind::Flags(declared) => {
                // Which flags are set is one decision, so the visitor answers
                // with the whole set rather than being asked per flag.
                let spec = visitor.on_flags(&declared)?;
                self.write(&spec)
            }
            leaf => {
                debug_assert!(
                    !leaf.is_composite(),
                    "composite kind `{}` reached the leaf arm",
                    leaf.name()
                );
                // The visitor supplies content as a spec.
                // The walk writes it at this node's slot.
                let spec = match leaf {
                    Kind::Bool => visitor.on_bool(),
                    Kind::S8 => visitor.on_s8(),
                    Kind::S16 => visitor.on_s16(),
                    Kind::S32 => visitor.on_s32(),
                    Kind::S64 => visitor.on_s64(),
                    Kind::U8 => visitor.on_u8(),
                    Kind::U16 => visitor.on_u16(),
                    Kind::U32 => visitor.on_u32(),
                    Kind::U64 => visitor.on_u64(),
                    Kind::F32 => visitor.on_f32(),
                    Kind::F64 => visitor.on_f64(),
                    Kind::Char => visitor.on_char(),
                    Kind::String => visitor.on_string(),
                    other => visitor.on_other(other.name()),
                }?;
                self.write(&spec)
            }
        }
    }

    /// A constant element index as a `u32` value, for the position callbacks
    /// of a sequence whose length is known while emitting. The runtime loops
    /// pass their counter instead. An index is not a position in WIT, but a
    /// number the walk computes for a visitor.
    fn index_value(&self, index: usize) -> Result<Value> {
        let ty = self.ty.child(wit_parser::Type::U32);
        let slot = reserve(&self.ty.ctx, &self.emitter, ty.wit())?;
        let value = Value::new(ty, slot, self.emitter.clone());
        value.write(&ValueSpec::u32(index as u32))?;
        Ok(value)
    }

    /// Build a variant-like value: ask the visitor for a value that holds the
    /// case index at runtime, then emit the same dispatch the read side emits.
    /// Every case's body is emitted, and that value selects one.
    fn write_variant(&self, visitor: &mut dyn WriteVisitor) -> Result<()> {
        let (cases, ..) = self.ty.variant_cases()?;
        let names: Vec<&str> = cases.iter().map(|(name, _)| name.as_str()).collect();
        let case_index = visitor.case_index(&names)?;
        let disc = self.local(ValType::I32);
        case_index.push()?;
        self.emit(Instruction::LocalSet(disc));
        self.write_cases(&cases, 0, disc, visitor)
    }

    /// The if/else chain over a variant's cases in the write direction, the
    /// mirror of [`Value::read_cases`]. Each branch writes its case into this
    /// value: the discriminant, then the payload if it carries one.
    fn write_cases(
        &self,
        cases: &[(String, Option<wit_parser::Type>)],
        case: usize,
        disc: u32,
        visitor: &mut dyn WriteVisitor,
    ) -> Result<()> {
        // The case name is not needed: the discriminant is written directly.
        let Some((_, payload)) = cases.get(case) else {
            return Ok(());
        };
        let payload = *payload;
        let write_case = |this: &Self, visitor: &mut dyn WriteVisitor| -> Result<()> {
            let (_, tag, payload_offset) = this.ty.variant_cases()?;
            let payload_slot = this.write_disc(tag, case as i64, payload_offset)?;
            if let Some(payload) = payload {
                visitor.begin_payload()?;
                this.at_slot(this.ty.child(payload), payload_slot)
                    .write_walk(visitor)?;
                visitor.end_payload()?;
            }
            Ok(())
        };
        // The last case needs no test. Every other case has been ruled out.
        if case + 1 == cases.len() {
            return write_case(self, visitor);
        }
        self.emit(Instruction::LocalGet(disc));
        self.emit(Instruction::I32Const(case as i32));
        self.emit(Instruction::I32Eq);
        self.emitter
            .if_(BlockType::Empty, || write_case(self, visitor))?
            .else_(|| self.write_cases(cases, case + 1, disc, visitor))
    }

    /// Build a list or map: ask the visitor how many elements there are,
    /// allocate room for them, then emit a loop that builds one entry per
    /// iteration. Finally write the `{ptr, len}` pair into this value.
    ///
    /// `members` is the entry's shape: one type for a list, two for a map.
    ///
    /// This is the one place the write walk allocates, since a sequence's
    /// elements live in their own heap block.
    fn write_elements(
        &self,
        members: &[wit_parser::Type],
        visitor: &mut dyn WriteVisitor,
    ) -> Result<()> {
        let layout = self.ty.ctx.layout();
        let stride = layout.record_size(members.iter());
        let offsets = layout.field_offsets(members.iter());

        let count = visitor.length()?;
        let length = self.local(ValType::I32);
        count.push()?;
        self.emit(Instruction::LocalSet(length));

        let pointer = self.local(ValType::I32);
        call_allocator(
            &self.ty.ctx,
            &self.emitter,
            Size::Strided {
                count: Local::new(length, ValType::I32),
                stride,
            },
        );
        self.emit(Instruction::LocalSet(pointer));

        let index = self.local(ValType::I32);
        let entry = self.local(ValType::I32);
        self.emit(Instruction::I32Const(0));
        self.emit(Instruction::LocalSet(index));
        let members = members.to_vec();
        self.block("element_done", || {
            self.loop_("element_loop", || {
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::LocalGet(length));
                self.emit(Instruction::I32GeU);
                self.br_if("element_done")?;
                self.emit(Instruction::LocalGet(pointer));
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::I32Const(stride as i32));
                self.emit(Instruction::I32Mul);
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalSet(entry));
                // The position is a runtime value here, the loop counter,
                // rather than a constant known while emitting.
                let position = Value::new(
                    self.ty.child(wit_parser::Type::U32),
                    Slot::flat(vec![Local::new(index, ValType::I32)]),
                    self.emitter.clone(),
                );
                visitor.begin_element(&position)?;
                for (ty, offset) in members.iter().zip(&offsets) {
                    self.at_base(self.ty.child(*ty), entry, *offset)
                        .write_walk(visitor)?;
                }
                visitor.end_element()?;
                self.emit(Instruction::LocalGet(index));
                self.emit(Instruction::I32Const(1));
                self.emit(Instruction::I32Add);
                self.emit(Instruction::LocalSet(index));
                self.br("element_loop")
            })
        })?;

        // This value is the pair, and both halves are runtime locals here.
        self.writer().write_ptr_len(
            &self.slot,
            Local::new(pointer, ValType::I32),
            Len::In(Local::new(length, ValType::I32)),
        )
    }
}

/// What a [`Type`] is, decomposed one level. A consumer matches this
/// exhaustively and recurses into child types.
pub enum Kind {
    Bool,
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
    F32,
    F64,
    Char,
    String,
    ErrorContext,
    Record(Vec<Field>),
    Variant(Vec<Case>),
    Enum(Vec<String>),
    Flags(Vec<String>),
    Option(Type),
    Result {
        ok: Option<Type>,
        err: Option<Type>,
    },
    List(Type),
    /// `list<T, N>`, a fixed number of elements laid out inline.
    FixedLengthList(Type, u32),
    Tuple(Vec<Type>),
    /// `map<K, V>`, as a list of pairs.
    Map(Type, Type),
    /// `future<T>`, or `None` for a parameterless `future`.
    Future(Option<Type>),
    /// `stream<T>`, or `None` for a parameterless `stream`.
    Stream(Option<Type>),
    /// A handle to a resource, owned or borrowed.
    Handle(Type),
    Resource,
}

impl Kind {
    /// Whether this kind has children to descend into, which is what decides
    /// whether a walk recurses or terminates.
    pub fn is_composite(&self) -> bool {
        matches!(
            self,
            Kind::Record(_)
                | Kind::Variant(_)
                | Kind::Option(_)
                | Kind::Result { .. }
                | Kind::Enum(_)
                | Kind::List(_)
                | Kind::FixedLengthList(_, _)
                | Kind::Tuple(_)
                | Kind::Map(_, _)
        )
    }

    /// Whether a runtime tag selects one of several cases. `variant`,
    /// `option`, `result`, and `enum` share one walk, dispatching on a tag.
    /// `enum` has no payload, but its case name is a per-branch constant, so
    /// producing it also requires the dispatch.
    pub fn is_variant_like(&self) -> bool {
        matches!(
            self,
            Kind::Variant(_) | Kind::Option(_) | Kind::Result { .. } | Kind::Enum(_)
        )
    }

    /// This kind's WIT name.
    fn name(&self) -> &'static str {
        match self {
            Kind::Bool => "bool",
            Kind::U8 => "u8",
            Kind::U16 => "u16",
            Kind::U32 => "u32",
            Kind::U64 => "u64",
            Kind::S8 => "s8",
            Kind::S16 => "s16",
            Kind::S32 => "s32",
            Kind::S64 => "s64",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Char => "char",
            Kind::String => "string",
            Kind::ErrorContext => "error-context",
            Kind::Record(_) => "record",
            Kind::Variant(_) => "variant",
            Kind::Enum(_) => "enum",
            Kind::Flags(_) => "flags",
            Kind::Option(_) => "option",
            Kind::Result { .. } => "result",
            Kind::List(_) => "list",
            Kind::FixedLengthList(..) => "fixed-length-list",
            Kind::Tuple(_) => "tuple",
            Kind::Map(..) => "map",
            Kind::Future(_) => "future",
            Kind::Stream(_) => "stream",
            Kind::Handle(_) => "handle",
            Kind::Resource => "resource",
        }
    }
}

/// One arm of a [`Value::dispatch`]: a case name, and the body to emit when
/// the dispatched value carries that case. The body receives the case's
/// payload, or `None` for a payload-less case. Anything else it needs, it
/// captures from its enclosing scope.
pub struct MatchArm<'f> {
    case: String,
    body: Box<dyn FnOnce(Option<Value>) -> Result<()> + 'f>,
}

/// Build a [`MatchArm`] for `case`, emitting `body` inside that case's branch.
pub fn arm<'f>(
    case: impl Into<String>,
    body: impl FnOnce(Option<Value>) -> Result<()> + 'f,
) -> MatchArm<'f> {
    MatchArm {
        case: case.into(),
        body: Box::new(body),
    }
}

/// The resource type behind a handle, following aliases. `None` for anything
/// that is not a handle.
fn handle_resource(resolve: &Resolve, ty: wit_parser::Type) -> Option<wit_parser::TypeId> {
    let wit_parser::Type::Id(id) = ty else {
        return None;
    };
    match &resolve.types[id].kind {
        TypeDefKind::Handle(wit_parser::Handle::Own(id) | wit_parser::Handle::Borrow(id)) => {
            Some(*id)
        }
        TypeDefKind::Type(inner) => handle_resource(resolve, *inner),
        _ => None,
    }
}

/// How a scalar of `from` becomes one of `to`: `Some(instruction)` when a
/// conversion must be emitted, `None` when the two share a core representation
/// and the value passes through.
///
/// The WIT kinds indicate signedness, so `i32 -> i64` is `extend_s` or
/// `extend_u` based on whether the source is signed.
fn coercion(from: &Kind, to: &Kind) -> Result<Option<Instruction<'static>>> {
    use Kind::*;
    /// Width and domain of a scalar kind, as the table needs to see it.
    enum Repr {
        /// A narrow integer, and whether it is signed.
        I32(bool),
        /// A 64-bit integer, and whether it is signed.
        I64(bool),
        F32,
        F64,
        /// `bool`, `string`, a handle: no numeric conversion is defined.
        Same,
    }
    let classify = |kind: &Kind| match kind {
        S8 | S16 | S32 => Repr::I32(true),
        U8 | U16 | U32 | Char => Repr::I32(false),
        S64 => Repr::I64(true),
        U64 => Repr::I64(false),
        F32 => Repr::F32,
        F64 => Repr::F64,
        _ => Repr::Same,
    };
    Ok(match (classify(from), classify(to)) {
        // Non-numeric on both sides.
        (Repr::Same, Repr::Same) => None,
        (Repr::I32(_), Repr::I32(_))
        | (Repr::I64(_), Repr::I64(_))
        | (Repr::F32, Repr::F32)
        | (Repr::F64, Repr::F64) => None,
        // Int width changes, sign-extending when the source is signed.
        (Repr::I32(signed), Repr::I64(_)) => Some(if signed {
            Instruction::I64ExtendI32S
        } else {
            Instruction::I64ExtendI32U
        }),
        (Repr::I64(_), Repr::I32(_)) => Some(Instruction::I32WrapI64),
        (Repr::F32, Repr::F64) => Some(Instruction::F64PromoteF32),
        (Repr::F64, Repr::F32) => Some(Instruction::F32DemoteF64),
        // Int to float: no trapping case, unlike float to int.
        (Repr::I32(true), Repr::F32) => Some(Instruction::F32ConvertI32S),
        (Repr::I32(false), Repr::F32) => Some(Instruction::F32ConvertI32U),
        (Repr::I32(true), Repr::F64) => Some(Instruction::F64ConvertI32S),
        (Repr::I32(false), Repr::F64) => Some(Instruction::F64ConvertI32U),
        (Repr::I64(true), Repr::F32) => Some(Instruction::F32ConvertI64S),
        (Repr::I64(false), Repr::F32) => Some(Instruction::F32ConvertI64U),
        (Repr::I64(true), Repr::F64) => Some(Instruction::F64ConvertI64S),
        (Repr::I64(false), Repr::F64) => Some(Instruction::F64ConvertI64U),
        // Float to int has two forms, trapping and saturating, and the types
        // do not say which, so a factory emits the one it intends.
        _ => bail!(
            "coerce: no conversion defined from `{}` to `{}`{}",
            from.name(),
            to.name(),
            if matches!(classify(from), Repr::F32 | Repr::F64)
                && matches!(classify(to), Repr::I32(_) | Repr::I64(_))
            {
                ": float-to-int must choose trapping or saturating truncation, so it is not \
                  derived; emit the form you intend"
            } else {
                ""
            }
        ),
    })
}

/// Route a leaf to its per-kind [`ReadVisitor`] method.
fn dispatch_leaf(kind: Kind, value: Value, visitor: &mut dyn ReadVisitor) -> Result<()> {
    match kind {
        Kind::Bool => visitor.on_bool(value),
        Kind::S8 => visitor.on_s8(value),
        Kind::S16 => visitor.on_s16(value),
        Kind::S32 => visitor.on_s32(value),
        Kind::S64 => visitor.on_s64(value),
        Kind::U8 => visitor.on_u8(value),
        Kind::U16 => visitor.on_u16(value),
        Kind::U32 => visitor.on_u32(value),
        Kind::U64 => visitor.on_u64(value),
        Kind::F32 => visitor.on_f32(value),
        Kind::F64 => visitor.on_f64(value),
        Kind::Char => visitor.on_char(value),
        Kind::String => visitor.on_string(value),
        other => visitor.on_other(other.name(), value),
    }
}

/// Reports each node of an existing value as it is walked.
///
/// Leaves arrive at per-kind methods, and composites bracket with begin/end
/// pairs. Every callback returns `()`, so whatever a visitor accumulates is
/// its own to expose. Every per-kind method defaults to `on_other`, which
/// errors, so an unhandled kind fails unless the visitor overrides `on_other`.
pub trait ReadVisitor {
    /// Fallback for any leaf kind not handled. Errors by default.
    fn on_other(&mut self, kind: &str, _value: Value) -> Result<()> {
        bail!("unhandled leaf kind `{kind}` (override the per-kind method or `on_other`)")
    }

    fn on_bool(&mut self, value: Value) -> Result<()> {
        self.on_other("bool", value)
    }

    fn on_s8(&mut self, value: Value) -> Result<()> {
        self.on_other("s8", value)
    }

    fn on_s16(&mut self, value: Value) -> Result<()> {
        self.on_other("s16", value)
    }

    fn on_s32(&mut self, value: Value) -> Result<()> {
        self.on_other("s32", value)
    }

    fn on_s64(&mut self, value: Value) -> Result<()> {
        self.on_other("s64", value)
    }

    fn on_u8(&mut self, value: Value) -> Result<()> {
        self.on_other("u8", value)
    }

    fn on_u16(&mut self, value: Value) -> Result<()> {
        self.on_other("u16", value)
    }

    fn on_u32(&mut self, value: Value) -> Result<()> {
        self.on_other("u32", value)
    }

    fn on_u64(&mut self, value: Value) -> Result<()> {
        self.on_other("u64", value)
    }

    fn on_f32(&mut self, value: Value) -> Result<()> {
        self.on_other("f32", value)
    }

    fn on_f64(&mut self, value: Value) -> Result<()> {
        self.on_other("f64", value)
    }

    fn on_char(&mut self, value: Value) -> Result<()> {
        self.on_other("char", value)
    }

    fn on_string(&mut self, value: Value) -> Result<()> {
        self.on_other("string", value)
    }

    fn begin_record(&mut self) -> Result<()> {
        Ok(())
    }

    fn end_record(&mut self) -> Result<()> {
        Ok(())
    }

    fn begin_field(&mut self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn end_field(&mut self) -> Result<()> {
        Ok(())
    }

    fn begin_list(&mut self) -> Result<()> {
        Ok(())
    }

    fn end_list(&mut self) -> Result<()> {
        Ok(())
    }

    fn begin_tuple(&mut self) -> Result<()> {
        Ok(())
    }

    fn end_tuple(&mut self) -> Result<()> {
        Ok(())
    }

    /// Followed by one `begin_case` / `end_case` pair per declared case.
    fn begin_variant(&mut self) -> Result<()> {
        Ok(())
    }

    fn begin_case(&mut self, _name: &str, _has_payload: bool) -> Result<()> {
        Ok(())
    }

    fn end_case(&mut self) -> Result<()> {
        Ok(())
    }

    fn end_variant(&mut self) -> Result<()> {
        Ok(())
    }

    /// Followed by one `on_flag` per declared flag, inside a test of its bit.
    fn begin_flags(&mut self) -> Result<()> {
        Ok(())
    }

    fn on_flag(&mut self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn end_flags(&mut self) -> Result<()> {
        Ok(())
    }

    fn begin_map(&mut self) -> Result<()> {
        Ok(())
    }

    fn end_map(&mut self) -> Result<()> {
        Ok(())
    }
}

/// Supplies the content for a value being built, as its type is walked.
///
/// The inverse of [`ReadVisitor`]: leaves return a [`ValueSpec`] rather than
/// receiving a value. Members are bracketed, but containers are not, since the
/// visitor is asked for content at a position whose type it already knows.
///
/// Two callbacks return a [`Value`] rather than a spec, because their answer
/// is only known when the component runs: how many elements a sequence has,
/// and which case of a variant applies.
pub trait WriteVisitor {
    /// Fallback for any leaf kind not handled. Errors by default.
    fn on_other(&mut self, kind: &str) -> Result<ValueSpec> {
        bail!("unhandled leaf kind `{kind}` (override the per-kind method or `on_other`)")
    }

    fn on_bool(&mut self) -> Result<ValueSpec> {
        self.on_other("bool")
    }

    fn on_s8(&mut self) -> Result<ValueSpec> {
        self.on_other("s8")
    }

    fn on_s16(&mut self) -> Result<ValueSpec> {
        self.on_other("s16")
    }

    fn on_s32(&mut self) -> Result<ValueSpec> {
        self.on_other("s32")
    }

    fn on_s64(&mut self) -> Result<ValueSpec> {
        self.on_other("s64")
    }

    fn on_u8(&mut self) -> Result<ValueSpec> {
        self.on_other("u8")
    }

    fn on_u16(&mut self) -> Result<ValueSpec> {
        self.on_other("u16")
    }

    fn on_u32(&mut self) -> Result<ValueSpec> {
        self.on_other("u32")
    }

    fn on_u64(&mut self) -> Result<ValueSpec> {
        self.on_other("u64")
    }

    fn on_f32(&mut self) -> Result<ValueSpec> {
        self.on_other("f32")
    }

    fn on_f64(&mut self) -> Result<ValueSpec> {
        self.on_other("f64")
    }

    fn on_char(&mut self) -> Result<ValueSpec> {
        self.on_other("char")
    }

    fn on_string(&mut self) -> Result<ValueSpec> {
        self.on_other("string")
    }

    /// Supplies a `flags` value: which of `declared` are set.
    fn on_flags(&mut self, _declared: &[String]) -> Result<ValueSpec> {
        self.on_other("flags")
    }

    fn begin_field(&mut self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn end_field(&mut self) -> Result<()> {
        Ok(())
    }

    /// Called for each element of a sequence. `index` is a constant when the
    /// length is known while emitting, otherwise the loop counter, which is
    /// only valid within this call.
    fn begin_element(&mut self, _index: &Value) -> Result<()> {
        Ok(())
    }

    fn end_element(&mut self) -> Result<()> {
        Ok(())
    }

    fn begin_payload(&mut self) -> Result<()> {
        Ok(())
    }

    fn end_payload(&mut self) -> Result<()> {
        Ok(())
    }

    /// How many elements in the sequence being built. A [`Value`], not a
    /// number, because it's only known at runtime.
    fn length(&mut self) -> Result<Value> {
        bail!("producing a list requires `length` (this visitor does not implement it)")
    }

    /// Which case of the variant being built applies, as an index into
    /// `names`. A [`Value`] for the same reason as [`WriteVisitor::length`].
    fn case_index(&mut self, _names: &[&str]) -> Result<Value> {
        bail!("producing a variant requires `case_index` (this visitor does not implement it)")
    }
}

/// A record field.
#[derive(Clone)]
pub struct Field {
    name: String,
    ty: Type,
}

impl Field {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn ty(&self) -> Type {
        self.ty.clone()
    }
}

/// A variant case.
#[derive(Clone)]
pub struct Case {
    name: String,
    payload: Option<Type>,
}

impl Case {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn payload(&self) -> Option<Type> {
        self.payload.clone()
    }
}

/// The interfaces and functions a generated component may call.
#[derive(Clone)]
pub struct Imports {
    ctx: Rc<BuildContext>,
    emitter: Emitter,
}

impl Imports {
    pub(crate) fn new(ctx: Rc<BuildContext>, emitter: Emitter) -> Self {
        Imports { ctx, emitter }
    }

    /// An imported interface by name.
    pub fn interface(&self, name: &str) -> Result<Interface> {
        let world = &self.ctx.resolve().worlds[self.ctx.world()];
        world
            .imports
            .iter()
            .find_map(|(key, item)| match item {
                WorldItem::Interface { id, .. }
                    if interface_matches(self.ctx.resolve(), key, name) =>
                {
                    Some(Interface {
                        ctx: Rc::clone(&self.ctx),
                        emitter: self.emitter.clone(),
                        key: key.clone(),
                        id: *id,
                    })
                }
                _ => None,
            })
            .ok_or_else(|| anyhow!("no imported interface '{name}'"))
    }

    /// Every imported interface, in declaration order.
    pub fn interfaces(&self) -> Vec<Interface> {
        self.ctx.resolve().worlds[self.ctx.world()]
            .imports
            .iter()
            .filter_map(|(key, item)| match item {
                WorldItem::Interface { id, .. } => Some(Interface {
                    ctx: Rc::clone(&self.ctx),
                    emitter: self.emitter.clone(),
                    key: key.clone(),
                    id: *id,
                }),
                _ => None,
            })
            .collect()
    }

    /// An imported function declared directly by the world.
    pub fn function(&self, name: &str) -> Result<ImportedFunction> {
        self.ctx.resolve().worlds[self.ctx.world()]
            .imports
            .values()
            .find_map(|item| match item {
                WorldItem::Function(func) if func.name == name => {
                    Some(self.callable(None, func.clone()))
                }
                _ => None,
            })
            .ok_or_else(|| anyhow!("no imported function '{name}'"))?
    }

    /// Every function the world imports directly, in declaration order. An
    /// interface's functions are reached through [`Interface::functions`].
    pub fn functions(&self) -> Result<Vec<ImportedFunction>> {
        self.ctx.resolve().worlds[self.ctx.world()]
            .imports
            .values()
            .filter_map(|item| match item {
                WorldItem::Function(func) => Some(self.callable(None, func.clone())),
                _ => None,
            })
            .collect()
    }

    /// Pair a declared function with its core index.
    fn callable(
        &self,
        interface: Option<&WorldKey>,
        func: wit_parser::Function,
    ) -> Result<ImportedFunction> {
        let index = abi::import_index(self.ctx.imports(), interface, &func.name)
            .ok_or_else(|| anyhow!("imported function '{}' has no core index", func.name))?;
        Ok(ImportedFunction {
            ctx: Rc::clone(&self.ctx),
            emitter: self.emitter.clone(),
            func: Rc::new(func),
            index,
        })
    }
}

/// An imported interface.
#[derive(Clone)]
pub struct Interface {
    ctx: Rc<BuildContext>,
    emitter: Emitter,
    key: WorldKey,
    id: wit_parser::InterfaceId,
}

impl Interface {
    /// The interface's leaf name, absent if anonymous.
    pub fn name(&self) -> Option<&str> {
        self.ctx.resolve().interfaces[self.id].name.as_deref()
    }

    /// A function of this interface by name.
    pub fn function(&self, name: &str) -> Result<ImportedFunction> {
        let func = self.ctx.resolve().interfaces[self.id]
            .functions
            .get(name)
            .ok_or_else(|| anyhow!("no function '{name}' in this interface"))?
            .clone();
        self.callable(func)
    }

    /// Every function of this interface, in declaration order.
    pub fn functions(&self) -> Result<Vec<ImportedFunction>> {
        self.ctx.resolve().interfaces[self.id]
            .functions
            .values()
            .cloned()
            .map(|func| self.callable(func))
            .collect()
    }

    /// Pair a declared function with its core index.
    fn callable(&self, func: wit_parser::Function) -> Result<ImportedFunction> {
        let index = abi::import_index(self.ctx.imports(), Some(&self.key), &func.name)
            .ok_or_else(|| anyhow!("imported function '{}' has no core index", func.name))?;
        Ok(ImportedFunction {
            ctx: Rc::clone(&self.ctx),
            emitter: self.emitter.clone(),
            func: Rc::new(func),
            index,
        })
    }
}

/// A function the generated component may call.
#[derive(Clone)]
pub struct ImportedFunction {
    ctx: Rc<BuildContext>,
    emitter: Emitter,
    func: Rc<wit_parser::Function>,
    index: u32,
}

impl ImportedFunction {
    pub fn name(&self) -> &str {
        &self.func.name
    }

    pub fn is_async(&self) -> bool {
        self.func.kind.is_async()
    }

    /// Call this function with `args`, returning a result if it declares one.
    ///
    /// Each argument must flatten to what its param expects: widening and
    /// narrowing are the caller's responsibility, through [`Value::coerce`].
    ///
    /// Calling an async import is not yet supported.
    pub fn call(&self, args: &[Value]) -> Result<Option<Value>> {
        if self.func.kind.is_async() {
            bail!(
                "call: '{}' is an async import, which is not yet supported; an async call \
                 returns a status code and fills its result in later, so its result cannot be \
                 read where a sync one can",
                self.func.name
            );
        }
        let resolve = self.ctx.resolve();
        let expected: Vec<Vec<ValType>> = self
            .func
            .params
            .iter()
            .map(|param| abi::flat_types(resolve, param.ty))
            .collect::<Result<_>>()?;
        if args.len() != expected.len() {
            bail!(
                "call: '{}' expects {} args, got {}",
                self.func.name,
                expected.len(),
                args.len()
            );
        }
        for (index, (arg, expected)) in args.iter().zip(&expected).enumerate() {
            let got = abi::flat_types(resolve, arg.ty().wit())?;
            if &got != expected {
                bail!(
                    "call: '{}' arg {index} type mismatch: the value flattens to {got:?} but the \
                     param expects {expected:?} (coerce the value to match)",
                    self.func.name
                );
            }
        }
        // The retarea must be allocated before the args are pushed, so that
        // the args are the ones at the top of the stack when the call runs.
        let retarea = abi::import_returns_indirectly(resolve, &self.func)
            .then(|| {
                let ty = self
                    .func
                    .result
                    .ok_or_else(|| anyhow!("call: an indirect result has no type"))?;
                let base = self.emitter.local(ValType::I32);
                call_allocator(
                    &self.ctx,
                    &self.emitter,
                    Size::Const(self.ctx.layout().size(&ty)),
                );
                self.emitter.emit(Instruction::LocalSet(base));
                Ok::<u32, anyhow::Error>(base)
            })
            .transpose()?;
        for arg in args {
            arg.push()?;
        }
        if let Some(base) = retarea {
            self.emitter.emit(Instruction::LocalGet(base));
        }
        self.emitter.emit(Instruction::Call(self.index));
        let Some(result_ty) = self.result_type() else {
            return Ok(None);
        };
        let slot = match retarea {
            // The callee wrote into the area it was passed.
            Some(base) => Slot::at(base),
            // The result came back on the stack, so set it into a local.
            None => {
                let flats = abi::flat_types(resolve, result_ty.wit())?;
                let core = flats.first().copied().unwrap_or(ValType::I32);
                let local = Local::new(self.emitter.local(core), core);
                self.emitter.emit(Instruction::LocalSet(local.index));
                Slot::flat(vec![local])
            }
        };
        Ok(Some(Value::new(result_ty, slot, self.emitter.clone())))
    }

    /// The type of a declared param.
    pub fn param(&self, name: &str) -> Result<ImportedFunctionParam> {
        self.func
            .params
            .iter()
            .find(|param| param.name == name)
            .map(|param| self.argument(param))
            .ok_or_else(|| anyhow!("no param '{name}' on '{}'", self.func.name))
    }

    /// Every param, in declaration order.
    pub fn params(&self) -> Vec<ImportedFunctionParam> {
        self.func
            .params
            .iter()
            .map(|param| self.argument(param))
            .collect()
    }

    /// The declared result type, or `None` for a function with no result.
    pub fn result_type(&self) -> Option<Type> {
        self.func
            .result
            .map(|ty| Type::new(Rc::clone(&self.ctx), ty))
    }

    fn argument(&self, param: &wit_parser::Param) -> ImportedFunctionParam {
        ImportedFunctionParam {
            ctx: Rc::clone(&self.ctx),
            emitter: self.emitter.clone(),
            name: param.name.clone(),
            ty: param.ty,
        }
    }
}

/// A declared parameter on an imported or exported function.
pub trait Param {
    fn name(&self) -> &str;
    fn ty(&self) -> Type;
}

/// A parameter of an imported function, callable with an argument value.
#[derive(Clone)]
pub struct ImportedFunctionParam {
    ctx: Rc<BuildContext>,
    emitter: Emitter,
    name: String,
    ty: wit_parser::Type,
}

impl Param for ImportedFunctionParam {
    fn name(&self) -> &str {
        &self.name
    }

    fn ty(&self) -> Type {
        Type::new(Rc::clone(&self.ctx), self.ty)
    }
}

impl ImportedFunctionParam {
    /// The writable value for an argument to pass. Created fresh for each
    /// call, since an imported function may be called more than once with
    /// different arguments.
    pub fn value(&self) -> Result<Value> {
        let slot = reserve(&self.ctx, &self.emitter, self.ty)?;
        Ok(Value::new(
            <Self as Param>::ty(self),
            slot,
            self.emitter.clone(),
        ))
    }
}

/// Where an exported function's result is delivered from. Reserved before
/// anything else is emitted for that function's body.
#[derive(Clone)]
pub struct FunctionResult {
    value: Value,
    /// Whether this result is returned through a pointer.
    indirect: bool,
}

impl FunctionResult {
    pub(crate) fn new(value: Value, indirect: bool) -> Self {
        FunctionResult { value, indirect }
    }

    /// Whether the core result is a pointer.
    pub(crate) fn indirect(&self) -> bool {
        self.indirect
    }

    /// The value this function delivers: write into it, or hand it to the arms
    /// of a [`Value::dispatch`] as their shared destination.
    pub fn value(&self) -> Value {
        self.value.clone()
    }
}

/// A function the generated component implements.
#[derive(Clone)]
pub struct ExportedFunction {
    ctx: Rc<BuildContext>,
    emitter: Emitter,
    /// The interface this export belongs to, or `None` if world-level.
    interface: Option<WorldKey>,
    func: Rc<wit_parser::Function>,
    /// This function's result destination, or `None` if no result.
    result: Option<FunctionResult>,
}

impl ExportedFunction {
    pub(crate) fn new(
        ctx: Rc<BuildContext>,
        emitter: Emitter,
        interface: Option<WorldKey>,
        func: Rc<wit_parser::Function>,
    ) -> Self {
        ExportedFunction {
            ctx,
            emitter,
            interface,
            func,
            result: None,
        }
    }

    /// The instruction stream of the body being generated, available whenever
    /// emitting directly is preferable to using value operations.
    pub fn body(&self) -> Emitter {
        self.emitter.clone()
    }

    /// Where this function's result is delivered from, or `None` if no result.
    pub fn result(&self) -> Option<FunctionResult> {
        self.result.clone()
    }

    /// The underlying WIT declaration of this function.
    pub(crate) fn wit(&self) -> &wit_parser::Function {
        &self.func
    }

    /// Set the result holder for this function.
    pub(crate) fn set_result(&mut self, result: FunctionResult) {
        self.result = Some(result);
    }

    pub fn name(&self) -> &str {
        &self.func.name
    }

    /// The fully qualified name of the interface this export belongs to, or
    /// `None` for a world-level export.
    pub fn qualified_interface_name(&self) -> Option<String> {
        self.interface
            .as_ref()
            .map(|key| self.ctx.resolve().name_world_key(key))
    }

    pub fn is_async(&self) -> bool {
        self.func.kind.is_async()
    }

    /// A declared param by name.
    pub fn param(&self, name: &str) -> Result<ExportedFunctionParam> {
        self.func
            .params
            .iter()
            .enumerate()
            .find(|(_, param)| param.name == name)
            .map(|(index, param)| self.received(index, param))
            .ok_or_else(|| anyhow!("no param '{name}' on '{}'", self.func.name))
    }

    /// Every param, in declaration order.
    pub fn params(&self) -> Vec<ExportedFunctionParam> {
        self.func
            .params
            .iter()
            .enumerate()
            .map(|(index, param)| self.received(index, param))
            .collect()
    }

    /// The declared result type, or `None` for a function with no result.
    pub fn result_type(&self) -> Option<Type> {
        self.func
            .result
            .map(|ty| Type::new(Rc::clone(&self.ctx), ty))
    }

    fn received(&self, index: usize, param: &wit_parser::Param) -> ExportedFunctionParam {
        ExportedFunctionParam {
            ctx: Rc::clone(&self.ctx),
            emitter: self.emitter.clone(),
            func: Rc::clone(&self.func),
            index,
            name: param.name.clone(),
            ty: param.ty,
        }
    }
}

/// A parameter of an exported function, receivable as an argument value.
#[derive(Clone)]
pub struct ExportedFunctionParam {
    ctx: Rc<BuildContext>,
    emitter: Emitter,
    /// The owning signature, read to place this param among the core locals.
    func: Rc<wit_parser::Function>,
    index: usize,
    name: String,
    ty: wit_parser::Type,
}

impl Param for ExportedFunctionParam {
    fn name(&self) -> &str {
        &self.name
    }

    fn ty(&self) -> Type {
        Type::new(Rc::clone(&self.ctx), self.ty)
    }
}

impl ExportedFunctionParam {
    /// The argument value passed from a caller. Nothing is emitted or
    /// allocated by calling this function, since the argument already exists
    /// in the body's locals.
    pub fn receive(&self) -> Result<Value> {
        let resolve = self.ctx.resolve();
        let mut first = 0u32;
        for earlier in &self.func.params[..self.index] {
            first += abi::flat_types(resolve, earlier.ty)?.len() as u32;
        }
        let locals = abi::flat_types(resolve, self.ty)?
            .into_iter()
            .enumerate()
            .map(|(offset, core)| Local::new(first + offset as u32, core))
            .collect();
        Ok(Value::new(
            <Self as Param>::ty(self),
            Slot::flat(locals),
            self.emitter.clone(),
        ))
    }
}

/// Whether `key` matches the interface `name` refers to, by either short name
/// or by qualified `pkg:ns/name`.
fn interface_matches(resolve: &Resolve, key: &WorldKey, name: &str) -> bool {
    match key {
        WorldKey::Name(key_name) => key_name == name,
        WorldKey::Interface(id) => {
            resolve.interfaces[*id].name.as_deref() == Some(name)
                || resolve.name_world_key(key) == name
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wit_parser::WorldId;

    /// The context for `wit`'s sole world.
    fn context(wit: &str) -> Rc<BuildContext> {
        let (resolve, world) = world(wit);
        Rc::new(BuildContext::new(resolve, world))
    }

    fn world(wit: &str) -> (Rc<Resolve>, WorldId) {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("test.wit", wit).expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");
        (Rc::new(resolve), world)
    }

    /// The type of the sole param of the sole imported function.
    fn param_type(wit: &str) -> Type {
        let ctx = context(wit);
        let (resolve, world) = (ctx.resolve(), ctx.world());
        let (_, func) = resolve.worlds[world]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].functions.iter().next()
                }
                _ => None,
            })
            .expect("an imported function");
        Type::new(Rc::clone(&ctx), func.params[0].ty)
    }

    /// Records the callback sequence a walk produces, so a test asserts on the
    /// shape reported rather than on emitted bytes.
    #[derive(Default)]
    struct Recorder {
        events: Vec<String>,
    }

    impl Recorder {
        fn note(&mut self, event: impl Into<String>) -> Result<()> {
            self.events.push(event.into());
            Ok(())
        }
    }

    impl ReadVisitor for Recorder {
        fn on_other(&mut self, kind: &str, _value: Value) -> Result<()> {
            self.note(format!("leaf:{kind}"))
        }
        fn begin_record(&mut self) -> Result<()> {
            self.note("record{")
        }
        fn end_record(&mut self) -> Result<()> {
            self.note("}")
        }
        fn begin_field(&mut self, name: &str) -> Result<()> {
            self.note(format!("field:{name}"))
        }
        fn begin_list(&mut self) -> Result<()> {
            self.note("list[")
        }
        fn end_list(&mut self) -> Result<()> {
            self.note("]")
        }
        fn begin_tuple(&mut self) -> Result<()> {
            self.note("tuple(")
        }
        fn end_tuple(&mut self) -> Result<()> {
            self.note(")")
        }
        fn begin_variant(&mut self) -> Result<()> {
            self.note("variant<")
        }
        fn begin_case(&mut self, name: &str, has_payload: bool) -> Result<()> {
            self.note(format!("case:{name}{}", if has_payload { "+" } else { "" }))
        }
        fn end_variant(&mut self) -> Result<()> {
            self.note(">")
        }
        fn begin_flags(&mut self) -> Result<()> {
            self.note("flags{")
        }
        fn on_flag(&mut self, name: &str) -> Result<()> {
            self.note(format!("flag:{name}"))
        }
        fn end_flags(&mut self) -> Result<()> {
            self.note("}")
        }
        fn begin_map(&mut self) -> Result<()> {
            self.note("map{")
        }
        fn end_map(&mut self) -> Result<()> {
            self.note("}")
        }
    }

    /// Walk a named type's value and return the callbacks it reported. The
    /// body is validated too, so a walk emitting invalid wasm fails here.
    fn walk(wit: &str, type_name: &str) -> Vec<String> {
        let ctx = context(wit);
        let ty = named_type_in(&ctx, type_name);
        let emitter = Emitter::new(1);
        let value = Value::new(ty, Slot::at(0), emitter.clone());
        let mut recorder = Recorder::default();
        value.read_with(&mut recorder).expect("walk");
        let function = emitter.encode().expect("encode");
        validate_body(&ctx, function, vec![ValType::I32]);
        recorder.events
    }

    /// A named type, from an already-built context.
    fn named_type_in(ctx: &Rc<BuildContext>, name: &str) -> Type {
        let (resolve, world) = (ctx.resolve(), ctx.world());
        let id = resolve.worlds[world]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].types.get(name).copied()
                }
                _ => None,
            })
            .expect("the declared type");
        Type::new(Rc::clone(ctx), wit_parser::Type::Id(id))
    }

    /// Wrap a walked body in a module with a memory and validate it.
    fn validate_body(
        ctx: &Rc<BuildContext>,
        function: wasm_encoder::Function,
        params: Vec<ValType>,
    ) {
        let (types, strings) = ctx.take_module_state();
        let module = crate::module::CoreModule {
            imports: Vec::new(),
            functions: vec![crate::module::CoreFunction {
                params,
                results: Vec::new(),
                body: function,
                export_name: "walk".to_string(),
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
            .expect("the emitted walk must be valid wasm");
    }

    #[test]
    fn a_record_walk_visits_each_field_in_order() {
        let events = walk(
            r"package test:walkrec;
              interface i { record point { x: u32, y: u64 } f: func(p: point); }
              world w { import i; }",
            "point",
        );
        assert_eq!(
            events,
            ["record{", "field:x", "leaf:u32", "field:y", "leaf:u64", "}"]
        );
    }

    #[test]
    fn a_nested_record_descends_into_its_members() {
        let events = walk(
            r"package test:walknest;
              interface i {
                record inner { n: u32 }
                record outer { a: inner, b: bool }
                f: func(o: outer);
              }
              world w { import i; }",
            "outer",
        );
        assert_eq!(
            events,
            [
                "record{",
                "field:a",
                "record{",
                "field:n",
                "leaf:u32",
                "}",
                "field:b",
                "leaf:bool",
                "}"
            ]
        );
    }

    #[test]
    fn a_tuple_reports_members_without_names() {
        let events = walk(
            r"package test:walktup;
              interface i { type pair = tuple<u32, string>; f: func(p: pair); }
              world w { import i; }",
            "pair",
        );
        assert_eq!(events, ["tuple(", "leaf:u32", "leaf:string", ")"]);
    }

    #[test]
    fn a_list_reports_its_element_once_inside_the_loop() {
        // The element walk is emitted once, inside a runtime loop, so the
        // visitor sees one element whatever the length turns out to be.
        let events = walk(
            r"package test:walklist;
              interface i { type nums = list<u32>; f: func(n: nums); }
              world w { import i; }",
            "nums",
        );
        assert_eq!(events, ["list[", "leaf:u32", "]"]);
    }

    #[test]
    fn a_fixed_length_list_walks_every_element() {
        // The length is known while emitting, so every element is reported.
        let events = walk(
            r"package test:walkfixed;
              interface i { type three = list<u32, 3>; f: func(t: three); }
              world w { import i; }",
            "three",
        );
        assert_eq!(events, ["list[", "leaf:u32", "leaf:u32", "leaf:u32", "]"]);
    }

    #[test]
    fn a_map_reports_each_entry_as_a_tuple() {
        let events = walk(
            r"package test:walkmap;
              interface i { type table = map<string, u32>; f: func(t: table); }
              world w { import i; }",
            "table",
        );
        assert_eq!(
            events,
            ["map{", "tuple(", "leaf:string", "leaf:u32", ")", "}"]
        );
    }

    #[test]
    fn a_variant_reports_every_case() {
        let events = walk(
            r"package test:walkvar;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
            "shape",
        );
        assert_eq!(
            events,
            ["variant<", "case:circle+", "leaf:u32", "case:empty", ">"]
        );
    }

    #[test]
    fn an_option_reports_none_and_some() {
        let events = walk(
            r"package test:walkopt;
              interface i { type maybe = option<u32>; f: func(m: maybe); }
              world w { import i; }",
            "maybe",
        );
        assert_eq!(
            events,
            ["variant<", "case:none", "case:some+", "leaf:u32", ">"]
        );
    }

    #[test]
    fn a_result_reports_both_cases_with_their_payloads() {
        let events = walk(
            r"package test:walkres;
              interface i { type outcome = result<u32, string>; f: func(o: outcome); }
              world w { import i; }",
            "outcome",
        );
        assert_eq!(
            events,
            [
                "variant<",
                "case:ok+",
                "leaf:u32",
                "case:err+",
                "leaf:string",
                ">"
            ]
        );
    }

    #[test]
    fn an_enum_reports_its_cases_without_payloads() {
        let events = walk(
            r"package test:walkenum;
              interface i { enum color { red, green } f: func(c: color); }
              world w { import i; }",
            "color",
        );
        assert_eq!(events, ["variant<", "case:red", "case:green", ">"]);
    }

    #[test]
    fn flags_report_each_declared_name() {
        let events = walk(
            r"package test:walkflags;
              interface i { flags perms { read, write } f: func(p: perms); }
              world w { import i; }",
            "perms",
        );
        assert_eq!(events, ["flags{", "flag:read", "flag:write", "}"]);
    }

    // Core opcodes used in these tests.
    const LOCAL_GET: u8 = 0x20;
    const LOCAL_SET: u8 = 0x21;
    const CALL: u8 = 0x10;
    const END: u8 = 0x0B;
    const I32_CONST: u8 = 0x41;
    const I32_LOAD: u8 = 0x28;
    const I32_LOAD8U: u8 = 0x2D;
    const I32_LOAD16U: u8 = 0x2F;
    const I32_ADD: u8 = 0x6A;
    const I32_MUL: u8 = 0x6C;

    /// The raw body bytes a walk emits.
    fn walk_bytes(wit: &str, type_name: &str) -> Vec<u8> {
        let ctx = context(wit);
        let ty = named_type_in(&ctx, type_name);
        let emitter = Emitter::new(1);
        let value = Value::new(ty, Slot::at(0), emitter.clone());
        value.read_with(&mut Recorder::default()).expect("walk");
        emitter.encode().expect("encode").into_raw_body()
    }

    /// The offsets every load of `opcode` addresses.
    fn load_offsets(bytes: &[u8], opcode: u8) -> Vec<u8> {
        bytes
            .windows(3)
            .filter(|window| window[0] == opcode)
            .map(|window| window[2])
            .collect()
    }

    #[test]
    fn a_discriminant_is_read_at_its_tag_width() {
        // Two cases fit a u8 tag: reading four bytes would pull in padding and
        // payload, and still report exactly the same cases.
        let bytes = walk_bytes(
            r"package test:discread;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
            "shape",
        );
        assert!(
            bytes.contains(&I32_LOAD8U),
            "the discriminant is a one-byte load: {bytes:02x?}"
        );
    }

    #[test]
    fn a_variant_payload_is_read_past_its_discriminant() {
        // `circle`'s u32 payload is 4-aligned, so it sits at 4, not at 0 where
        // the discriminant is.
        let bytes = walk_bytes(
            r"package test:payloadread;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
            "shape",
        );
        assert!(
            bytes.windows(2).any(|w| w[0] == I32_CONST && w[1] == 4),
            "the payload is addressed at its own offset: {bytes:02x?}"
        );
    }

    #[test]
    fn pointer_and_length_are_read_from_adjacent_slots() {
        // `{pointer, length}` is two i32s: reading the length at the pointer's
        // offset would use the address as the loop count.
        let bytes = walk_bytes(
            r"package test:ptrlenread;
              interface i { type nums = list<u32>; f: func(n: nums); }
              world w { import i; }",
            "nums",
        );
        let offsets = load_offsets(&bytes, I32_LOAD);
        assert!(
            offsets.contains(&0) && offsets.contains(&4),
            "pointer at 0 and length at 4: {offsets:?}"
        );
    }

    #[test]
    fn flags_are_read_at_their_repr_width() {
        // Two flags fit a u8 repr; a full-word load would read past the value.
        let narrow = walk_bytes(
            r"package test:flagnarrow;
              interface i { flags perms { read, write } f: func(p: perms); }
              world w { import i; }",
            "perms",
        );
        assert!(
            narrow.contains(&I32_LOAD8U),
            "a 2-flag bitset is a one-byte load: {narrow:02x?}"
        );

        // Nine flags need a u16.
        let wide = walk_bytes(
            r"package test:flagwide;
              interface i {
                flags many { a, b, c, d, e, f, g, h, i }
                check: func(m: many);
              }
              world w { import i; }",
            "many",
        );
        assert!(
            wide.contains(&I32_LOAD16U),
            "a 9-flag bitset is a two-byte load: {wide:02x?}"
        );
    }

    #[test]
    fn a_memory_leaf_is_rebased_onto_its_own_pointer() {
        // A leaf must arrive self-contained rather than addressed relative to
        // its parent. `{ x: u32, y: u64 }` puts y at 8, so the walk computes
        // base + 8 and makes that the leaf's base.
        let bytes = walk_bytes(
            r"package test:leafbase;
              interface i { record point { x: u32, y: u64 } f: func(p: point); }
              world w { import i; }",
            "point",
        );
        assert!(
            bytes
                .windows(3)
                .any(|w| w[0] == I32_CONST && w[1] == 8 && w[2] == I32_ADD),
            "y's offset is added to compute the new base: {bytes:02x?}"
        );
    }

    #[test]
    fn a_dispatched_discriminant_is_copied_out_of_the_value() {
        // Each arm runs implementor code that may write into the dispatched
        // value. Testing the value's own local would let one arm change what
        // the arms after it compare against, so the discriminant is copied to
        // a local of its own first.
        let ctx = context(
            r"package test:disccopy;
              interface i { variant v { a(u32), b(u32), c } f: func(x: v); }
              world w { import i; }",
        );
        let ty = named_type_in(&ctx, "v");
        let emitter = Emitter::new(0);
        let locals: Vec<Local> = [ValType::I32, ValType::I32]
            .iter()
            .map(|core| Local::new(emitter.local(*core), *core))
            .collect();
        let own = locals[0].index as u8;
        let value = Value::new(ty, Slot::flat(locals), emitter.clone());
        value
            .dispatch(vec![
                arm("a", |_| Ok(())),
                arm("b", |_| Ok(())),
                arm("c", |_| Ok(())),
            ])
            .expect("dispatch");
        let bytes = emitter.encode().expect("encode").into_raw_body();
        // The value's own local is read exactly once, to make the copy.
        // Every arm test then reads the copy.
        let reads_own = bytes
            .windows(2)
            .filter(|window| window[0] == LOCAL_GET && window[1] == own)
            .count();
        assert_eq!(
            reads_own, 1,
            "the discriminant is copied once, not re-read per arm: {bytes:02x?}"
        );
    }

    #[test]
    fn a_flat_variant_hands_its_payload_the_local_after_the_discriminant() {
        // Flattened, a variant is `[disc, ...joined payload]`. Reporting a
        // flat leaf emits nothing, so the payload slot is observable only by
        // pushing it: a slot that still included the discriminant would
        // `local.get` the tag's local.
        struct PushesItsLeaf;
        impl ReadVisitor for PushesItsLeaf {
            fn on_u32(&mut self, value: Value) -> Result<()> {
                value.push()
            }
            fn on_other(&mut self, _kind: &str, _value: Value) -> Result<()> {
                Ok(())
            }
        }
        let ctx = context(
            r"package test:flatvar;
              interface i { variant v { n(u32), empty } f: func(x: v); }
              world w { import i; }",
        );
        let ty = named_type_in(&ctx, "v");
        let emitter = Emitter::new(0);
        let locals: Vec<Local> = [ValType::I32, ValType::I32]
            .iter()
            .map(|core| Local::new(emitter.local(*core), *core))
            .collect();
        let (disc_local, payload_local) = (locals[0].index as u8, locals[1].index as u8);
        let value = Value::new(ty, Slot::flat(locals), emitter.clone());
        value.read_with(&mut PushesItsLeaf).expect("walk");
        let bytes = emitter.encode().expect("encode").into_raw_body();
        let reads: Vec<u8> = bytes
            .windows(2)
            .filter(|window| window[0] == LOCAL_GET)
            .map(|window| window[1])
            .collect();
        assert!(
            reads.contains(&payload_local),
            "the payload is pushed from local {payload_local}: reads were {reads:?}"
        );
        assert_eq!(
            reads.iter().filter(|read| **read == disc_local).count(),
            1,
            "the discriminant is read only to dispatch on: {reads:?}"
        );
    }

    /// Answers every leaf with a fixed spec and records the positions it was
    /// asked about, so a test asserts on the shape the walk requested.
    struct Supplier {
        events: Vec<String>,
        /// Which case `case_index` names and how long a sequence claims to be.
        case: usize,
        length: u32,
        /// Set when a value is needed for a runtime answer.
        ctx: Rc<BuildContext>,
        emitter: Emitter,
    }

    impl Supplier {
        fn new(ctx: &Rc<BuildContext>, emitter: &Emitter) -> Self {
            Supplier {
                events: Vec::new(),
                case: 0,
                length: 1,
                ctx: Rc::clone(ctx),
                emitter: emitter.clone(),
            }
        }

        fn note(&mut self, event: impl Into<String>) {
            self.events.push(event.into());
        }

        /// A `u32` value holding `n`, for the callbacks that answer with one.
        fn number(&self, n: u32) -> Result<Value> {
            let ty = Type::new(Rc::clone(&self.ctx), wit_parser::Type::U32);
            let slot = reserve(&self.ctx, &self.emitter, ty.wit())?;
            let value = Value::new(ty, slot, self.emitter.clone());
            value.write(&ValueSpec::u32(n))?;
            Ok(value)
        }
    }

    impl WriteVisitor for Supplier {
        fn on_other(&mut self, kind: &str) -> Result<ValueSpec> {
            self.note(format!("leaf:{kind}"));
            Ok(match kind {
                "u32" => ValueSpec::u32(1),
                "u64" => ValueSpec::u64(2),
                "bool" => ValueSpec::bool(true),
                "string" => ValueSpec::string("s"),
                other => bail!("test supplier has no value for {other}"),
            })
        }
        fn on_flags(&mut self, declared: &[String]) -> Result<ValueSpec> {
            self.note(format!("flags?{}", declared.join(",")));
            Ok(ValueSpec::flags(declared.last().cloned()))
        }
        fn begin_field(&mut self, name: &str) -> Result<()> {
            self.note(format!("field:{name}"));
            Ok(())
        }
        fn begin_element(&mut self, _index: &Value) -> Result<()> {
            self.note("element");
            Ok(())
        }
        fn begin_payload(&mut self) -> Result<()> {
            self.note("payload");
            Ok(())
        }
        fn length(&mut self) -> Result<Value> {
            self.note("length?");
            self.number(self.length)
        }
        fn case_index(&mut self, names: &[&str]) -> Result<Value> {
            self.note(format!("case?{}", names.join(",")));
            self.number(self.case as u32)
        }
    }

    /// Build a named type's value from a supplier, returning what it was asked
    /// for. The emitted body is validated, so an invalid walk fails here.
    fn build(wit: &str, type_name: &str) -> Vec<String> {
        let ctx = context(wit);
        let ty = named_type_in(&ctx, type_name);
        let emitter = Emitter::new(1);
        let slot = reserve(&ctx, &emitter, ty.wit()).expect("reserve");
        let value = Value::new(ty, slot, emitter.clone());
        let mut supplier = Supplier::new(&ctx, &emitter);
        value.write_with(&mut supplier).expect("build");
        let function = emitter.encode().expect("encode");
        validate_with_allocator(&ctx, function, vec![ValType::I32]);
        supplier.events
    }

    /// Like `validate_body` but with the "alloc" import the write walk calls.
    fn validate_with_allocator(
        ctx: &Rc<BuildContext>,
        function: wasm_encoder::Function,
        params: Vec<ValType>,
    ) {
        let (types, strings) = ctx.take_module_state();
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
            // The canonical ABI's realloc signature, which is what the real
            // module declares.
            params: vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            results: vec![ValType::I32],
        });
        let module = crate::module::CoreModule {
            imports,
            functions: vec![crate::module::CoreFunction {
                params,
                results: Vec::new(),
                body: function,
                export_name: "build".to_string(),
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
            .expect("the emitted build must be valid wasm");
    }

    #[test]
    fn a_record_asks_for_each_field_in_order() {
        let events = build(
            r"package test:buildrec;
              interface i { record point { x: u32, y: u64 } f: func(p: point); }
              world w { import i; }",
            "point",
        );
        assert_eq!(
            events,
            ["field:x", "leaf:u32", "field:y", "leaf:u64"],
            "no begin_record: the write side brackets only positions it descends into"
        );
    }

    #[test]
    fn flags_are_asked_for_once_with_their_declared_names() {
        // Which flags are set is one decision, unlike the read side, which
        // tests each declared flag at runtime.
        let events = build(
            r"package test:buildflags;
              interface i { flags perms { read, write } f: func(p: perms); }
              world w { import i; }",
            "perms",
        );
        assert_eq!(events, ["flags?read,write"]);
    }

    #[test]
    fn the_flags_the_visitor_names_are_the_ones_written() {
        // The supplier names the last declared flag, so the bitset is bit 4.
        let bytes = build_bytes(
            r"package test:flagbits;
              interface i {
                flags perms { read, write, exec, share, admin }
                f: func(p: perms);
              }
              world w { import i; }",
            "perms",
        );
        assert!(
            bytes
                .windows(3)
                .any(|w| w[0] == I32_CONST && w[1] == 0b10000 && w[2] == LOCAL_SET),
            "the named flag's bit is set: {bytes:02x?}"
        );
    }

    #[test]
    fn a_nested_record_descends_asking_for_inner_fields() {
        let events = build(
            r"package test:buildnest;
              interface i {
                record inner { n: u32 }
                record outer { a: inner, b: bool }
                f: func(o: outer);
              }
              world w { import i; }",
            "outer",
        );
        assert_eq!(
            events,
            ["field:a", "field:n", "leaf:u32", "field:b", "leaf:bool"]
        );
    }

    #[test]
    fn a_tuple_asks_by_position() {
        let events = build(
            r"package test:buildtup;
              interface i { type pair = tuple<u32, string>; f: func(p: pair); }
              world w { import i; }",
            "pair",
        );
        assert_eq!(events, ["element", "leaf:u32", "element", "leaf:string"]);
    }

    #[test]
    fn a_fixed_length_list_asks_for_every_element() {
        let events = build(
            r"package test:buildfixed;
              interface i { type three = list<u32, 3>; f: func(t: three); }
              world w { import i; }",
            "three",
        );
        assert_eq!(
            events,
            [
                "element", "leaf:u32", "element", "leaf:u32", "element", "leaf:u32"
            ]
        );
    }

    #[test]
    fn a_list_asks_for_its_length_then_builds_one_element_in_the_loop() {
        // The element body is emitted once, inside a runtime loop, so the
        // supplier is asked about one element however long the list is.
        let events = build(
            r"package test:buildlist;
              interface i { type nums = list<u32>; f: func(n: nums); }
              world w { import i; }",
            "nums",
        );
        assert_eq!(events, ["length?", "element", "leaf:u32"]);
    }

    /// The raw body bytes a build emits.
    fn build_bytes(wit: &str, type_name: &str) -> Vec<u8> {
        let ctx = context(wit);
        let ty = named_type_in(&ctx, type_name);
        let emitter = Emitter::new(1);
        let slot = reserve(&ctx, &emitter, ty.wit()).expect("reserve");
        let value = Value::new(ty, slot, emitter.clone());
        let mut supplier = Supplier::new(&ctx, &emitter);
        value.write_with(&mut supplier).expect("build");
        emitter.encode().expect("encode").into_raw_body()
    }

    #[test]
    fn sequence_elements_are_strided_by_their_entry_size() {
        // Element `i` sits at `pointer + i * stride`. A zero stride would size
        // the allocation at nothing and write every element over the first,
        // while the callbacks stay the same and the wasm still validates.
        let bytes = build_bytes(
            r"package test:stride;
              interface i { type wide = list<u64>; f: func(w: wide); }
              world w { import i; }",
            "wide",
        );
        // `i32.const 8` followed by `i32.mul`.
        let strided = bytes
            .windows(3)
            .filter(|window| window[0] == I32_CONST && window[1] == 8 && window[2] == I32_MUL)
            .count();
        assert_eq!(
            strided, 2,
            "the stride sizes the allocation and addresses each entry: {bytes:02x?}"
        );
    }

    #[test]
    fn map_entries_are_strided_by_the_whole_pair() {
        // A `map<u32, u64>` entry is 8-aligned: 4 bytes of key, 4 of padding,
        // 8 of value. So the stride is 16, not 12.
        let bytes = build_bytes(
            r"package test:mapstride;
              interface i { type table = map<u32, u64>; f: func(t: table); }
              world w { import i; }",
            "table",
        );
        let strided = bytes
            .windows(3)
            .filter(|window| window[0] == I32_CONST && window[1] == 16 && window[2] == I32_MUL)
            .count();
        assert_eq!(
            strided, 2,
            "16 sizes the allocation and strides each entry: {bytes:02x?}"
        );
    }

    #[test]
    fn the_allocation_is_sized_by_the_runtime_length() {
        // `alloc(length * stride)`, not a fixed size: allocating for one entry
        // and then looping past it writes into whatever the allocator hands
        // out next, while the wasm still validates.
        let bytes = build_bytes(
            r"package test:allocsize;
              interface i { type nums = list<u32>; f: func(n: nums); }
              world w { import i; }",
            "nums",
        );
        // `local.get <length>`, `i32.const 4`, `i32.mul`.
        assert!(
            bytes
                .windows(5)
                .any(|w| w[0] == LOCAL_GET && w[2] == I32_CONST && w[3] == 4 && w[4] == I32_MUL),
            "the size is the length local times the stride: {bytes:02x?}"
        );
    }

    #[test]
    fn a_value_that_fits_is_reserved_in_locals() {
        // A `u32` is one flat, so reserving it emits nothing: locals are
        // declared in the header. Using memory would emit an allocator call.
        let ctx = context(
            r"package test:reserveflat;
              interface i { type n = u32; f: func(x: n); }
              world w { import i; }",
        );
        let emitter = Emitter::new(0);
        let ty = named_type_in(&ctx, "n");
        let slot = reserve(&ctx, &emitter, ty.wit()).expect("reserve");
        assert!(slot.base().is_none(), "a u32 lives in locals");
        // The body is the local declarations then `end`: no call, no store.
        let bytes = emitter.encode().expect("encode").into_raw_body();
        assert!(
            !bytes.contains(&CALL),
            "reserving locals calls nothing: {bytes:02x?}"
        );
        assert_eq!(
            bytes.last(),
            Some(&END),
            "and emits no instruction before `end`: {bytes:02x?}"
        );
    }

    #[test]
    fn a_value_too_wide_to_flatten_is_allocated_at_its_own_size() {
        // 17 u32 fields exceed `MAX_FLAT_PARAMS`, forcing the memory path. The
        // record occupies 68 bytes, and that is what the allocation requests.
        let ctx = context(
            r"package test:reservesize;
              interface i {
                record wide {
                  a: u32, b: u32, c: u32, d: u32, e: u32, f: u32,
                  g: u32, h: u32, i: u32, j: u32, k: u32, l: u32,
                  m: u32, n: u32, o: u32, p: u32, q: u32
                }
                read: func(w: wide);
              }
              world w { import i; }",
        );
        let ty = named_type_in(&ctx, "wide");
        assert_eq!(ctx.layout().size(&ty.wit()), 68);
        let emitter = Emitter::new(0);
        let slot = reserve(&ctx, &emitter, ty.wit()).expect("reserve");
        assert!(slot.base().is_some(), "17 flats force the memory path");
        let bytes = emitter.encode().expect("encode").into_raw_body();
        // 68 encodes as the two-byte LEB128 `c4 00`.
        assert!(
            bytes.windows(3).any(|w| w == [I32_CONST, 0xC4, 0x00]),
            "the type's own size is requested: {bytes:02x?}"
        );
    }

    #[test]
    fn a_map_asks_for_both_members_of_each_entry() {
        let events = build(
            r"package test:buildmap;
              interface i { type table = map<string, u32>; f: func(t: table); }
              world w { import i; }",
            "table",
        );
        assert_eq!(events, ["length?", "element", "leaf:string", "leaf:u32"]);
    }

    #[test]
    fn a_variant_asks_which_case_then_builds_every_branch() {
        // Every branch is emitted, since the selected case is only known at
        // runtime. Both cases are named, but only `circle` has a payload.
        let events = build(
            r"package test:buildvar;
              interface i { variant shape { circle(u32), empty } f: func(s: shape); }
              world w { import i; }",
            "shape",
        );
        assert_eq!(events, ["case?circle,empty", "payload", "leaf:u32"]);
    }

    #[test]
    fn an_option_asks_which_case_and_builds_the_some_payload() {
        let events = build(
            r"package test:buildopt;
              interface i { type maybe = option<u32>; f: func(m: maybe); }
              world w { import i; }",
            "maybe",
        );
        assert_eq!(events, ["case?none,some", "payload", "leaf:u32"]);
    }

    #[test]
    fn a_result_builds_for_both_cases() {
        let events = build(
            r"package test:buildres;
              interface i { type outcome = result<u32, string>; f: func(o: outcome); }
              world w { import i; }",
            "outcome",
        );
        assert_eq!(
            events,
            [
                "case?ok,err",
                "payload",
                "leaf:u32",
                "payload",
                "leaf:string"
            ]
        );
    }

    #[test]
    fn each_branch_writes_its_own_discriminant() {
        // Every arm writes the index of the case it builds.
        let bytes = build_bytes(
            r"package test:buildisc;
              interface i { enum color { red, green, blue } f: func(c: color); }
              world w { import i; }",
            "color",
        );
        // A 3-case enum is one flat, reserved in local 1, and each arm sets
        // its index there with `i32.const <n>` then `local.set`.
        let stored: Vec<u8> = bytes
            .windows(4)
            .filter(|window| window[0] == I32_CONST && window[2] == LOCAL_SET && window[3] == 1)
            .map(|window| window[1])
            .collect();
        assert_eq!(
            stored,
            [0, 1, 2],
            "each branch sets its own case index: {bytes:02x?}"
        );
    }

    #[test]
    fn an_enum_asks_which_case_and_needs_no_payload() {
        let events = build(
            r"package test:buildenum;
              interface i { enum color { red, green } f: func(c: color); }
              world w { import i; }",
            "color",
        );
        assert_eq!(events, ["case?red,green"]);
    }

    #[test]
    fn a_write_visitor_that_supplies_nothing_fails_on_the_first_leaf() {
        struct Nothing;
        impl WriteVisitor for Nothing {}
        let ctx = context(
            r"package test:buildstrict;
              interface i { type n = u32; f: func(x: n); }
              world w { import i; }",
        );
        let ty = named_type_in(&ctx, "n");
        let emitter = Emitter::new(1);
        let slot = reserve(&ctx, &emitter, ty.wit()).expect("reserve");
        let error = Value::new(ty, slot, emitter)
            .write_with(&mut Nothing)
            .expect_err("an unsupplied leaf must fail");
        assert!(format!("{error:#}").contains("u32"), "{error:#}");
    }

    #[test]
    fn a_list_without_a_length_is_reported() {
        // `length` has no default: a visitor that produces sequences must
        // implement it.
        struct NoLength;
        impl WriteVisitor for NoLength {
            fn on_other(&mut self, _kind: &str) -> Result<ValueSpec> {
                Ok(ValueSpec::u32(0))
            }
        }
        let ctx = context(
            r"package test:buildnolen;
              interface i { type nums = list<u32>; f: func(n: nums); }
              world w { import i; }",
        );
        let ty = named_type_in(&ctx, "nums");
        let emitter = Emitter::new(1);
        let slot = reserve(&ctx, &emitter, ty.wit()).expect("reserve");
        let error = Value::new(ty, slot, emitter)
            .write_with(&mut NoLength)
            .expect_err("a list requires a length");
        assert!(format!("{error:#}").contains("length"), "{error:#}");
    }

    #[test]
    fn a_read_visitor_that_handles_nothing_fails_on_the_first_leaf() {
        struct Nothing;
        impl ReadVisitor for Nothing {}
        let ctx = context(
            r"package test:walkstrict;
              interface i { type n = u32; f: func(x: n); }
              world w { import i; }",
        );
        let ty = named_type_in(&ctx, "n");
        let emitter = Emitter::new(1);
        let value = Value::new(ty, Slot::at(0), emitter);
        let error = value
            .read_with(&mut Nothing)
            .expect_err("an unhandled leaf must fail");
        assert!(format!("{error:#}").contains("u32"), "{error:#}");
    }

    /// A type declared in an interface, by name.
    fn named_type(wit: &str, name: &str) -> Type {
        let ctx = context(wit);
        let (resolve, world) = (ctx.resolve(), ctx.world());
        let id = resolve.worlds[world]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].types.get(name).copied()
                }
                _ => None,
            })
            .expect("the declared type");
        Type::new(Rc::clone(&ctx), wit_parser::Type::Id(id))
    }

    const PRIMITIVES: &str = r"package test:prims;
        interface i {
          f: func(a: bool, b: u8, c: u16, d: u32, e: u64, g: s8, h: s16,
                  i: s32, j: s64, k: f32, l: f64, m: char, n: string);
        }
        world w { import i; }";

    #[test]
    fn primitives_report_their_own_kind() {
        let ctx = context(PRIMITIVES);
        let (resolve, world) = (ctx.resolve(), ctx.world());
        let (_, func) = resolve.worlds[world]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].functions.iter().next()
                }
                _ => None,
            })
            .expect("the function");
        let kinds: Vec<&'static str> = func
            .params
            .iter()
            .map(|p| match Type::new(Rc::clone(&ctx), p.ty).kind() {
                Kind::Bool => "bool",
                Kind::U8 => "u8",
                Kind::U16 => "u16",
                Kind::U32 => "u32",
                Kind::U64 => "u64",
                Kind::S8 => "s8",
                Kind::S16 => "s16",
                Kind::S32 => "s32",
                Kind::S64 => "s64",
                Kind::F32 => "f32",
                Kind::F64 => "f64",
                Kind::Char => "char",
                Kind::String => "string",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "bool", "u8", "u16", "u32", "u64", "s8", "s16", "s32", "s64", "f32", "f64", "char",
                "string"
            ]
        );
    }

    #[test]
    fn primitives_have_no_name() {
        let ty = param_type(
            r"package test:noname; interface i { f: func(a: u32); } world w { import i; }",
        );
        assert_eq!(ty.name(), None);
    }

    #[test]
    fn a_declared_type_reports_its_name() {
        let ty = named_type(
            r"package test:named;
              interface i { record point { x: u32, y: u32 } f: func(p: point); }
              world w { import i; }",
            "point",
        );
        assert_eq!(ty.name(), Some("point"));
    }

    #[test]
    fn a_record_lists_its_fields_in_order() {
        let ty = named_type(
            r"package test:records;
              interface i { record point { x: u32, y: string } f: func(p: point); }
              world w { import i; }",
            "point",
        );
        let Kind::Record(fields) = ty.kind() else {
            panic!("expected a record");
        };
        let described: Vec<(&str, &str)> = fields
            .iter()
            .map(|f| (f.name(), f.ty().kind().name()))
            .collect();
        assert_eq!(described, vec![("x", "u32"), ("y", "string")]);
    }

    #[test]
    fn a_variant_reports_its_cases_and_payloads() {
        let ty = named_type(
            r"package test:variants;
              interface i { variant v { empty, full(u32) } f: func(x: v); }
              world w { import i; }",
            "v",
        );
        let Kind::Variant(cases) = ty.kind() else {
            panic!("expected a variant");
        };
        assert_eq!(cases[0].name(), "empty");
        assert!(cases[0].payload().is_none());
        assert_eq!(cases[1].name(), "full");
        assert!(matches!(
            cases[1].payload().expect("a payload").kind(),
            Kind::U32
        ));
    }

    #[test]
    fn an_enum_reports_its_case_names() {
        let ty = named_type(
            r"package test:enums;
              interface i { enum color { red, green } f: func(c: color); }
              world w { import i; }",
            "color",
        );
        let Kind::Enum(cases) = ty.kind() else {
            panic!("expected an enum");
        };
        assert_eq!(cases, vec!["red", "green"]);
    }

    #[test]
    fn flags_report_their_names() {
        let ty = named_type(
            r"package test:flagged;
              interface i { flags perms { read, write } f: func(p: perms); }
              world w { import i; }",
            "perms",
        );
        let Kind::Flags(names) = ty.kind() else {
            panic!("expected flags");
        };
        assert_eq!(names, vec!["read", "write"]);
    }

    #[test]
    fn containers_report_their_element_types() {
        let listed = param_type(
            r"package test:lists; interface i { f: func(a: list<u32>); } world w { import i; }",
        );
        assert!(matches!(listed.kind(), Kind::List(el) if matches!(el.kind(), Kind::U32)));

        let optional = param_type(
            r"package test:opts; interface i { f: func(a: option<string>); } world w { import i; }",
        );
        assert!(matches!(optional.kind(), Kind::Option(el) if matches!(el.kind(), Kind::String)));

        let tupled = param_type(
            r"package test:tuples; interface i { f: func(a: tuple<u32, bool>); } world w { import i; }",
        );
        let Kind::Tuple(members) = tupled.kind() else {
            panic!("expected a tuple");
        };
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn a_result_reports_both_arms() {
        let ty = param_type(
            r"package test:results;
              interface i { f: func(a: result<u32, string>); }
              world w { import i; }",
        );
        let Kind::Result { ok, err } = ty.kind() else {
            panic!("expected a result");
        };
        assert!(matches!(ok.expect("ok").kind(), Kind::U32));
        assert!(matches!(err.expect("err").kind(), Kind::String));
    }

    #[test]
    fn an_alias_reports_the_type_it_refers_to() {
        let ty = named_type(
            r"package test:alias;
              interface i { type count = u32; f: func(c: count); }
              world w { import i; }",
            "count",
        );
        // The alias keeps its own name but reports the underlying kind.
        assert_eq!(ty.name(), Some("count"));
        assert!(matches!(ty.kind(), Kind::U32));
    }

    #[test]
    fn a_handle_reports_the_resource_it_refers_to() {
        let ty = param_type(
            r"package test:handles;
              interface i { resource conn; f: func(c: borrow<conn>); }
              world w { import i; }",
        );
        let Kind::Handle(resource) = ty.kind() else {
            panic!("expected a handle");
        };
        assert_eq!(resource.name(), Some("conn"));
        assert!(matches!(resource.kind(), Kind::Resource));
    }

    fn imports(wit: &str) -> Imports {
        Imports::new(context(wit), Emitter::new(0))
    }

    const TWO_INTERFACES: &str = r"package test:nav;
        interface first { get: func() -> u32; }
        interface second { get: func() -> string; }
        world w { import first; import second; }";

    #[test]
    fn an_interface_is_found_by_short_name() {
        let imports = imports(TWO_INTERFACES);
        assert_eq!(imports.interface("first").unwrap().name(), Some("first"));
        assert_eq!(imports.interface("second").unwrap().name(), Some("second"));
    }

    #[test]
    fn an_unknown_interface_name_is_an_error() {
        let Err(error) = imports(TWO_INTERFACES).interface("third") else {
            panic!("an unknown interface must fail");
        };
        assert!(format!("{error:#}").contains("third"), "{error:#}");
    }

    #[test]
    fn interfaces_are_listed_in_declaration_order() {
        let listed: Vec<String> = imports(TWO_INTERFACES)
            .interfaces()
            .iter()
            .filter_map(|i| i.name().map(str::to_string))
            .collect();
        assert_eq!(listed, vec!["first", "second"]);
    }

    #[test]
    fn same_named_functions_get_their_own_signature() {
        let imports = imports(TWO_INTERFACES);
        let first = imports.interface("first").unwrap().function("get").unwrap();
        let second = imports
            .interface("second")
            .unwrap()
            .function("get")
            .unwrap();
        assert!(matches!(first.result_type().unwrap().kind(), Kind::U32));
        assert!(matches!(second.result_type().unwrap().kind(), Kind::String));
    }

    #[test]
    fn a_world_level_function_is_found_by_name() {
        let imports = imports(
            r"package test:worldlevel;
              interface iface { helper: func(); }
              world w { import iface; import solo: func(); }",
        );
        let solo = imports.function("solo").unwrap();
        assert_eq!(solo.name(), "solo");
    }

    #[test]
    fn world_level_functions_are_listed_in_declaration_order() {
        // An interface's functions are accessed via the interface, not here.
        let imports = imports(
            r"package test:worldlevellist;
              interface iface { helper: func(); }
              world w { import iface; import alpha: func(); import beta: func(); }",
        );
        let listed: Vec<String> = imports
            .functions()
            .unwrap()
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        assert_eq!(listed, vec!["alpha", "beta"]);
    }

    #[test]
    fn function_params_and_result_are_navigable() {
        let imports = imports(
            r"package test:params;
              interface iface { greet: func(name: string, times: u32) -> string; }
              world w { import iface; }",
        );
        let greet = imports
            .interface("iface")
            .unwrap()
            .function("greet")
            .unwrap();
        assert!(matches!(
            greet.param("name").unwrap().ty().kind(),
            Kind::String
        ));
        assert!(matches!(
            greet.param("times").unwrap().ty().kind(),
            Kind::U32
        ));
        assert_eq!(greet.params().len(), 2);
        assert!(matches!(greet.result_type().unwrap().kind(), Kind::String));
    }

    #[test]
    fn an_unknown_param_name_is_an_error() {
        let imports = imports(
            r"package test:noparam;
              interface iface { greet: func(name: string); }
              world w { import iface; }",
        );
        let greet = imports
            .interface("iface")
            .unwrap()
            .function("greet")
            .unwrap();
        let Err(error) = greet.param("absent") else {
            panic!("an unknown param must fail");
        };
        assert!(format!("{error:#}").contains("absent"), "{error:#}");
    }

    #[test]
    fn a_function_declaring_no_result_has_none() {
        let imports = imports(
            r"package test:noresult;
              interface iface { run: func(); }
              world w { import iface; }",
        );
        let run = imports.interface("iface").unwrap().function("run").unwrap();
        assert!(run.result_type().is_none());
        assert!(!run.is_async());
    }

    #[test]
    fn functions_are_listed_in_declaration_order() {
        let imports = imports(
            r"package test:listed;
              interface iface { alpha: func(); beta: func(); }
              world w { import iface; }",
        );
        let listed: Vec<String> = imports
            .interface("iface")
            .unwrap()
            .functions()
            .unwrap()
            .iter()
            .map(|f| f.name().to_string())
            .collect();
        assert_eq!(listed, vec!["alpha", "beta"]);
    }

    /// The sole exported function of a world.
    fn export(wit: &str) -> ExportedFunction {
        let ctx = context(wit);
        let (resolve, world) = (ctx.resolve(), ctx.world());
        let (key, func) = resolve.worlds[world]
            .exports
            .iter()
            .find_map(|(key, item)| match item {
                WorldItem::Interface { id, .. } => resolve.interfaces[*id]
                    .functions
                    .values()
                    .next()
                    .map(|func| (Some(key.clone()), func.clone())),
                WorldItem::Function(func) => Some((None, func.clone())),
                WorldItem::Type { .. } => None,
            })
            .expect("an exported function");
        ExportedFunction::new(Rc::clone(&ctx), Emitter::new(0), key, Rc::new(func))
    }

    #[test]
    fn an_export_reports_its_name_and_params() {
        let function = export(
            r"package test:exported;
              world w { export greet: func(name: string, times: u32) -> string; }",
        );
        assert_eq!(function.name(), "greet");
        assert!(!function.is_async());
        let declared = function.params();
        let params: Vec<&str> = declared.iter().map(|p| p.name()).collect();
        assert_eq!(params, vec!["name", "times"]);
        assert!(matches!(
            function.param("name").unwrap().ty().kind(),
            Kind::String
        ));
        assert!(matches!(
            function.param("times").unwrap().ty().kind(),
            Kind::U32
        ));
        assert!(matches!(
            function.result_type().unwrap().kind(),
            Kind::String
        ));
    }

    #[test]
    fn a_world_level_export_has_no_interface_name() {
        let function = export(r"package test:direct; world w { export run: func(); }");
        assert_eq!(function.qualified_interface_name(), None);
    }

    #[test]
    fn an_interface_export_reports_its_qualified_name() {
        let function = export(
            r"package test:iface;
              interface greeter { greet: func(); }
              world w { export greeter; }",
        );
        let qualified = function
            .qualified_interface_name()
            .expect("a qualified name");
        assert_eq!(qualified, "test:iface/greeter");
    }

    #[test]
    fn an_async_export_is_reported_as_async() {
        let function = export(r"package test:slow; world w { export wait: async func(); }");
        assert!(function.is_async());
    }

    #[test]
    fn every_kind_with_children_is_composite() {
        let wit = r"package test:composite;
            interface i {
              record r { a: u32 }
              variant v { one(u32), two }
              enum e { red }
              flags fl { read }
              type opt = option<u32>;
              type res = result<u32, string>;
              type lst = list<u32>;
              type fixed = list<u32, 4>;
              type tup = tuple<u32, u64>;
              type mp = map<string, u32>;
              f: func(a: u32);
            }
            world w { import i; }";
        for name in ["r", "v", "e", "opt", "res", "lst", "fixed", "tup", "mp"] {
            assert!(
                named_type(wit, name).kind().is_composite(),
                "{name} has children to descend into"
            );
        }
        // Flags are a bitset of names, not child types: nothing to recurse into.
        assert!(!named_type(wit, "fl").kind().is_composite());
    }

    #[test]
    fn no_primitive_is_composite() {
        let wit = r"package test:leafkinds;
            interface i { f: func(a: bool, b: u32, c: string, d: char, e: f64); }
            world w { import i; }";
        let ctx = context(wit);
        let (resolve, world) = (ctx.resolve(), ctx.world());
        let (_, func) = resolve.worlds[world]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].functions.iter().next()
                }
                _ => None,
            })
            .expect("an imported function");
        for param in &func.params {
            let kind = Type::new(Rc::clone(&ctx), param.ty).kind();
            assert!(!kind.is_composite(), "{} is a leaf", param.name);
            assert!(!kind.is_variant_like());
        }
    }

    #[test]
    fn only_tagged_kinds_are_variant_like() {
        let wit = r"package test:tagged;
            interface i {
              variant v { one(u32), two }
              enum e { red }
              record r { a: u32 }
              type opt = option<u32>;
              type res = result<u32, string>;
              type lst = list<u32>;
              f: func(a: u32);
            }
            world w { import i; }";
        // A runtime tag selects the case.
        for name in ["v", "e", "opt", "res"] {
            assert!(
                named_type(wit, name).kind().is_variant_like(),
                "{name} dispatches on a discriminant"
            );
        }
        // Composite, but every member is always present.
        for name in ["r", "lst"] {
            assert!(!named_type(wit, name).kind().is_variant_like(), "{name}");
        }
    }
}
