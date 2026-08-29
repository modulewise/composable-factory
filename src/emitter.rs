//! The Wasm instruction emitter for a function body.

use anyhow::{Result, anyhow};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_encoder::{BlockType, Function as EncodedFunction, Instruction, ValType};

/// A per-function instruction-emitting handle. Clones share a single
/// [`EmitterState`] so that a function body writer can hold several handles
/// but still emit a single stream of instructions.
#[derive(Clone)]
pub struct Emitter {
    state: Rc<RefCell<EmitterState>>,
}

/// State to support an [`Emitter`] whose public surface is its operations.
struct EmitterState {
    /// The instructions provided to `wasm_encoder` upon [`Emitter::encode`].
    instructions: Vec<Instruction<'static>>,
    /// The locals provided to `wasm_encoder` upon [`Emitter::encode`].
    locals: Vec<ValType>,
    /// Index where declared locals begin, after the core params.
    first_local: u32,
    /// Enclosing frame labels, ordered to capture depth. Pushed and popped
    /// while emitting; empty when the body is complete.
    labels: Vec<String>,
    /// An error detected where it cannot be returned (`drop` does not return a
    /// `Result`); surfaces in [`Emitter::encode`].
    deferred_error: Option<String>,
}

impl Emitter {
    /// Create an emitter for a single function body.
    pub fn new(param_count: u32) -> Self {
        Emitter {
            state: Rc::new(RefCell::new(EmitterState {
                instructions: Vec::new(),
                locals: Vec::new(),
                first_local: param_count,
                labels: Vec::new(),
                deferred_error: None,
            })),
        }
    }

    // The common callback for all operations, ensuring borrow-act-release.
    // Never expose the borrow itself; an accessor returning `RefMut` would
    // allow a caller to hold it across a callback.
    fn with_state<R>(&self, f: impl FnOnce(&mut EmitterState) -> R) -> R {
        match self.state.try_borrow_mut() {
            Ok(mut state) => f(&mut state),
            Err(_) => panic!(
                "the emitter is already in use; an operation ran while a \
                borrow was still open. Every operation borrows, acts, and \
                releases before returning, and the frame operations release \
                before running the passed closure."
            ),
        }
    }

    /// Append an instruction. Interleaves with any structured operations.
    pub fn emit(&self, instruction: Instruction<'static>) {
        self.with_state(|state| state.instructions.push(instruction));
    }

    /// An unconditional trap. Emits the `unreachable` instruction (opcode 0x00).
    pub fn trap(&self) {
        self.emit(Instruction::Unreachable);
    }

    /// Declare a local of `ty` and return its index.
    pub fn local(&self, ty: ValType) -> u32 {
        self.with_state(|state| {
            let index = state.first_local + state.locals.len() as u32;
            state.locals.push(ty);
            index
        })
    }

    /// Declare a labeled block and a closure for emitting body instructions.
    /// The `block_type` is the frame's stack signature. Branching to the label
    /// exits the block ("break").
    pub fn block<R>(
        &self,
        label: &str,
        block_type: BlockType,
        body: impl FnOnce() -> Result<R>,
    ) -> Result<R> {
        self.frame(Instruction::Block(block_type), label, body)
    }

    /// Declare a labeled loop and a closure for emitting body instructions.
    /// The `block_type` is the frame's stack signature. Branching to the label
    /// jumps to the start ("continue"). Otherwise, the loop exits by falling
    /// through.
    pub fn loop_<R>(
        &self,
        label: &str,
        block_type: BlockType,
        body: impl FnOnce() -> Result<R>,
    ) -> Result<R> {
        self.frame(Instruction::Loop(block_type), label, body)
    }

    /// Branch to a labeled frame.
    pub fn br(&self, label: &str) -> Result<()> {
        let depth = self.label_depth(label)?;
        self.emit(Instruction::Br(depth));
        Ok(())
    }

    /// Branch to a labeled frame if the (popped) top of the stack is non-zero.
    pub fn br_if(&self, label: &str) -> Result<()> {
        let depth = self.label_depth(label)?;
        self.emit(Instruction::BrIf(depth));
        Ok(())
    }

    /// An indexed branch: pops an index off the stack and branches to
    /// `targets[i]`, or to `default` if the index is out of range.
    pub fn br_table(&self, targets: &[&str], default: &str) -> Result<()> {
        let resolved = targets
            .iter()
            .map(|label| self.label_depth(label))
            .collect::<Result<Vec<u32>>>()?;
        let default_depth = self.label_depth(default)?;
        self.emit(Instruction::BrTable(resolved.into(), default_depth));
        Ok(())
    }

    /// An `if` block that tests (pops) the top of the stack for non-zero.
    pub fn if_(
        &self,
        block_type: BlockType,
        then_: impl FnOnce() -> Result<()>,
    ) -> Result<PendingElse> {
        self.with_state(|state| {
            state.instructions.push(Instruction::If(block_type));
            state.labels.push(String::new()); // anonymous, but tracks depth
        });
        match then_() {
            Ok(()) => Ok(PendingElse {
                emitter: self.clone(),
                block_type,
                closed: false,
            }),
            Err(error) => {
                self.close_frame();
                Err(error)
            }
        }
    }

    // The relative depth of an enclosing label where 0 is the innermost frame.
    fn label_depth(&self, label: &str) -> Result<u32> {
        self.with_state(|state| {
            state
                .labels
                .iter()
                .rev()
                .position(|l| l == label)
                .map(|from_top| from_top as u32)
                .ok_or_else(|| anyhow!("emitter: no enclosing label '{label}' in scope"))
        })
    }

    // Opens a frame, runs `body`, releases the borrowed emitter, and then
    // closes the frame. The `label` is pushed onto a stack to track its depth.
    fn frame<R>(
        &self,
        instruction: Instruction<'static>,
        label: &str,
        body: impl FnOnce() -> Result<R>,
    ) -> Result<R> {
        self.with_state(|state| {
            state.instructions.push(instruction);
            state.labels.push(label.to_string());
        });
        let result = body();
        // must close frame before returning
        self.close_frame();
        result
    }

    // Emits `end` and pops the label from the top of the stack.
    fn close_frame(&self) {
        self.with_state(|state| {
            state.labels.pop();
            state.instructions.push(Instruction::End);
        });
    }

    // Current number of open frames, tracked by labels on the stack.
    fn open_frame_count(&self) -> usize {
        self.with_state(|state| state.labels.len())
    }

    // Record the first of any errors that could not be returned (in a drop).
    fn defer_error(&self, message: String) {
        self.with_state(|state| {
            if state.deferred_error.is_none() {
                state.deferred_error = Some(message);
            }
        });
    }

    /// Encode the function by applying the locals and instructions. Every
    /// frame must be closed, so this can only be called between them.
    pub(crate) fn encode(&self) -> Result<EncodedFunction> {
        if let Some(error) = self.with_state(|state| state.deferred_error.clone()) {
            return Err(anyhow!("{error}"));
        }
        let open = self.open_frame_count();
        if open != 0 {
            return Err(anyhow!(
                "emitter: {open} control frame(s) left open at the end of the body"
            ));
        }
        Ok(self.with_state(|state| {
            let mut function = EncodedFunction::new_with_locals_types(state.locals.iter().copied());
            for instruction in &state.instructions {
                function.instruction(instruction);
            }
            function.instruction(&Instruction::End);
            function
        }))
    }
}

/// An `if` whose primary arm is emitted maintains an open frame so that an
/// `else` arm may optionally be provided. If the block type is non-empty, an
/// `else` arm is required. Otherwise, if not provided, the frame will still
/// close when this drops.
pub struct PendingElse {
    emitter: Emitter,
    block_type: BlockType,
    closed: bool,
}

impl PendingElse {
    /// The else-arm, emitted only after the then-arm's borrows are released.
    pub fn else_(mut self, body: impl FnOnce() -> Result<()>) -> Result<()> {
        self.emitter.emit(Instruction::Else);
        let result = body();
        self.emitter.close_frame();
        self.closed = true;
        result
    }
}

impl Drop for PendingElse {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.emitter.close_frame();
        // A non-Empty block type requires an else, because both arms must
        // leave a value of the same declared type. Record this error so that
        // it can be returned from `encode`.
        if !matches!(self.block_type, BlockType::Empty) {
            self.emitter.defer_error(
                "an `if` with a non-empty block type requires an `else` arm \
                to ensure a value of the declared type is produced"
                    .to_string(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Opcodes.
    const UNREACHABLE: u8 = 0x00;
    const NOP: u8 = 0x01;
    const BLOCK: u8 = 0x02;
    const LOOP: u8 = 0x03;
    const IF: u8 = 0x04;
    const ELSE: u8 = 0x05;
    const END: u8 = 0x0b;
    const BR: u8 = 0x0c;
    const BR_IF: u8 = 0x0d;
    const BR_TABLE: u8 = 0x0e;
    const I32_CONST: u8 = 0x41;

    // Value types, and the `BlockType::Empty` block-type operand.
    const I32: u8 = 0x7f;
    const I64: u8 = 0x7e;
    const EMPTY: u8 = 0x40;

    fn body(emitter: &Emitter) -> Vec<u8> {
        emitter.encode().expect("encode").into_raw_body()
    }

    fn validate(emitter: &Emitter, params: Vec<ValType>, results: Vec<ValType>) {
        use wasm_encoder::{CodeSection, FuncType, FunctionSection, Module, TypeSection};
        let function = emitter.encode().expect("encode");
        let mut types = TypeSection::new();
        types.ty().func_type(&FuncType::new(params, results));
        let mut functions = FunctionSection::new();
        functions.function(0);
        let mut code = CodeSection::new();
        code.function(&function);
        let mut module = Module::new();
        module.section(&types);
        module.section(&functions);
        module.section(&code);
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&module.finish())
            .expect("the emitted body must be valid wasm");
    }

    #[test]
    fn an_empty_body_has_end() {
        let emitter = Emitter::new(0);
        assert_eq!(body(&emitter), vec![0, END]);
    }

    #[test]
    fn trap_emits_unreachable() {
        let emitter = Emitter::new(0);
        emitter.trap();
        assert_eq!(body(&emitter), vec![0, UNREACHABLE, END]);
    }

    #[test]
    fn locals_are_indexed_after_params() {
        let emitter = Emitter::new(2);
        assert_eq!(emitter.local(ValType::I32), 2);
        assert_eq!(emitter.local(ValType::I64), 3);
    }

    #[test]
    fn locals_are_declared_in_order() {
        let emitter = Emitter::new(0);
        emitter.local(ValType::I32);
        emitter.local(ValType::I64);
        // group count, then each group as (count, type)
        assert_eq!(body(&emitter), vec![2, 1, I32, 1, I64, END]);
    }

    #[test]
    fn params_and_locals_are_addressable() {
        let emitter = Emitter::new(1);
        let val = emitter.local(ValType::I32);
        emitter.emit(Instruction::LocalGet(0));
        emitter.emit(Instruction::LocalSet(val));
        emitter.emit(Instruction::LocalGet(val));
        validate(&emitter, vec![ValType::I32], vec![ValType::I32]);
    }

    #[test]
    fn clones_share_instruction_stream() {
        let emitter = Emitter::new(0);
        let clone = emitter.clone();
        emitter.emit(Instruction::I32Const(1));
        clone.emit(Instruction::I32Const(2));
        assert_eq!(body(&emitter), vec![0, I32_CONST, 1, I32_CONST, 2, END]);
    }

    #[test]
    fn a_block_closes_its_frame() {
        let emitter = Emitter::new(0);
        emitter
            .block("done", BlockType::Empty, || Ok(()))
            .expect("block");
        assert_eq!(body(&emitter), vec![0, BLOCK, EMPTY, END, END]);
    }

    #[test]
    fn a_frame_closes_when_its_body_fails() {
        let emitter = Emitter::new(0);
        let result: Result<()> =
            emitter.block("done", BlockType::Empty, || anyhow::bail!("body failed"));
        assert!(result.is_err());
        assert!(
            emitter.encode().is_ok(),
            "the frame must be closed on failure"
        );
    }

    #[test]
    fn branching_resolves_the_innermost_label_as_zero() {
        let emitter = Emitter::new(0);
        emitter
            .block("outer", BlockType::Empty, || {
                emitter.block("inner", BlockType::Empty, || {
                    emitter.br("inner")?;
                    emitter.br("outer")
                })
            })
            .expect("nested blocks");
        let encoded = body(&emitter);
        let br_depths: Vec<u8> = encoded
            .windows(2)
            .filter(|w| w[0] == BR)
            .map(|w| w[1])
            .collect();
        assert_eq!(br_depths, vec![0, 1]);
        validate(&emitter, vec![], vec![]);
    }

    #[test]
    fn if_adds_to_depth_anonymously() {
        let emitter = Emitter::new(0);
        emitter
            .block("outer", BlockType::Empty, || {
                emitter.emit(Instruction::I32Const(1));
                emitter
                    .if_(BlockType::Empty, || emitter.br("outer"))?
                    .else_(|| Ok(()))
            })
            .expect("if inside block");
        let encoded = body(&emitter);
        let depth = encoded
            .windows(2)
            .find(|w| w[0] == BR)
            .map(|w| w[1])
            .expect("a br");
        assert_eq!(
            depth, 1,
            "the anonymous if frame bumps outer from depth 0 to 1"
        );
        validate(&emitter, vec![], vec![]);
    }

    #[test]
    fn loop_closes_its_frame() {
        let emitter = Emitter::new(0);
        emitter
            .loop_("again", BlockType::Empty, || Ok(()))
            .expect("loop");
        assert_eq!(body(&emitter), vec![0, LOOP, EMPTY, END, END]);
    }

    #[test]
    fn falling_out_of_a_loop_closes_its_frame() {
        let emitter = Emitter::new(0);
        emitter
            .loop_("again", BlockType::Empty, || {
                emitter.emit(Instruction::Nop);
                Ok(())
            })
            .expect("loop");
        assert_eq!(body(&emitter), vec![0, LOOP, EMPTY, NOP, END, END]);
    }

    #[test]
    fn br_if_encodes_its_target_depth() {
        let emitter = Emitter::new(0);
        emitter
            .block("done", BlockType::Empty, || {
                emitter.emit(Instruction::I32Const(1));
                emitter.br_if("done")
            })
            .expect("block");
        assert_eq!(
            body(&emitter),
            vec![0, BLOCK, EMPTY, I32_CONST, 1, BR_IF, 0, END, END]
        );
    }

    #[test]
    fn br_table_encodes_targets_then_default() {
        let emitter = Emitter::new(0);
        emitter
            .block("outer", BlockType::Empty, || {
                emitter.block("inner", BlockType::Empty, || {
                    emitter.emit(Instruction::I32Const(0));
                    emitter.br_table(&["inner", "outer"], "outer")
                })
            })
            .expect("nested blocks");
        let encoded = body(&emitter);
        let at = encoded
            .iter()
            .position(|b| *b == BR_TABLE)
            .expect("br_table");
        // opcode, target count, targets.., default
        assert_eq!(&encoded[at..at + 5], &[BR_TABLE, 2, 0, 1, 1]);
        validate(&emitter, vec![], vec![]);
    }

    #[test]
    fn loop_and_block_labels_resolve_their_depths() {
        let emitter = Emitter::new(0);
        emitter
            .block("done", BlockType::Empty, || {
                emitter.loop_("again", BlockType::Empty, || {
                    emitter.emit(Instruction::I32Const(1));
                    emitter.br_if("done")?;
                    emitter.br("again")
                })
            })
            .expect("loop in block");
        let encoded = body(&emitter);
        let br_if_depth = encoded
            .windows(2)
            .find(|w| w[0] == BR_IF)
            .map(|w| w[1])
            .expect("br_if");
        let br_depth = encoded
            .windows(2)
            .find(|w| w[0] == BR)
            .map(|w| w[1])
            .expect("br");
        assert_eq!(br_if_depth, 1, "the enclosing block is 1 frame deep");
        assert_eq!(br_depth, 0, "the loop is the innermost frame");
        validate(&emitter, vec![], vec![]);
    }

    #[test]
    fn nested_frames_resolve_by_depth() {
        let emitter = Emitter::new(0);
        emitter
            .block("outer", BlockType::Empty, || {
                emitter.loop_("middle", BlockType::Empty, || {
                    emitter.block("inner", BlockType::Empty, || {
                        emitter.br("inner")?;
                        emitter.br("middle")?;
                        emitter.br("outer")
                    })
                })
            })
            .expect("nested frames");
        let encoded = body(&emitter);
        let depths: Vec<u8> = encoded
            .windows(2)
            .filter(|w| w[0] == BR)
            .map(|w| w[1])
            .collect();
        assert_eq!(depths, vec![0, 1, 2]);
        validate(&emitter, vec![], vec![]);
    }

    #[test]
    fn if_without_else_is_ok_when_empty() {
        let emitter = Emitter::new(0);
        emitter.emit(Instruction::I32Const(1));
        drop(emitter.if_(BlockType::Empty, || Ok(())).expect("if"));
        assert!(emitter.encode().is_ok());
    }

    #[test]
    fn if_without_else_fails_when_a_value_is_declared() {
        let emitter = Emitter::new(0);
        emitter.emit(Instruction::I32Const(1));
        drop(
            emitter
                .if_(BlockType::Result(ValType::I32), || {
                    emitter.emit(Instruction::I32Const(7));
                    Ok(())
                })
                .expect("if"),
        );
        let error = emitter.encode().expect_err("a missing else must fail");
        assert!(error.to_string().contains("`else`"));
    }

    #[test]
    fn if_and_else_arms_must_align_when_yielding_a_value() {
        let emitter = Emitter::new(0);
        emitter.emit(Instruction::I32Const(1));
        emitter
            .if_(BlockType::Result(ValType::I32), || {
                emitter.emit(Instruction::I32Const(6));
                Ok(())
            })
            .expect("if")
            .else_(|| {
                emitter.emit(Instruction::I32Const(7));
                Ok(())
            })
            .expect("else");
        validate(&emitter, vec![], vec![ValType::I32]);
    }

    #[test]
    fn else_closes_its_frame() {
        let emitter = Emitter::new(0);
        emitter.emit(Instruction::I32Const(1));
        emitter
            .if_(BlockType::Empty, || Ok(()))
            .expect("if")
            .else_(|| Ok(()))
            .expect("else");
        assert_eq!(
            body(&emitter),
            vec![0, I32_CONST, 1, IF, EMPTY, ELSE, END, END]
        );
    }

    #[test]
    fn a_loop_that_yields_a_value_validates() {
        let emitter = Emitter::new(0);
        emitter
            .loop_("again", BlockType::Result(ValType::I32), || {
                emitter.emit(Instruction::I32Const(1));
                Ok(())
            })
            .expect("loop");
        validate(&emitter, vec![], vec![ValType::I32]);
    }

    #[test]
    fn branching_to_an_unknown_label_fails() {
        let emitter = Emitter::new(0);
        let result = emitter.block("done", BlockType::Empty, || emitter.br("nope"));
        assert!(result.is_err());
    }

    #[test]
    fn br_if_and_br_table_resolve_labels() {
        let emitter = Emitter::new(0);
        emitter
            .block("a", BlockType::Empty, || {
                emitter.block("b", BlockType::Empty, || {
                    emitter.emit(Instruction::I32Const(0));
                    emitter.br_if("a")?;
                    emitter.emit(Instruction::I32Const(0));
                    emitter.br_table(&["b", "a"], "a")
                })
            })
            .expect("nested blocks");
        validate(&emitter, vec![], vec![]);
    }

    #[test]
    fn br_table_rejects_an_unknown_target() {
        let emitter = Emitter::new(0);
        let result = emitter.block("a", BlockType::Empty, || {
            emitter.br_table(&["a", "nope"], "a")
        });
        assert!(result.is_err());
    }

    #[test]
    fn an_unclosed_frame_fails_at_encode() {
        let emitter = Emitter::new(0);
        // Reaching into the state directly because no operation can leak a frame.
        emitter.with_state(|state| state.labels.push("unclosed".to_string()));
        let error = emitter.encode().expect_err("an open frame must fail");
        assert!(error.to_string().contains("left open"));
    }

    #[test]
    fn the_first_deferred_error_wins() {
        let emitter = Emitter::new(0);
        emitter.defer_error("first".to_string());
        emitter.defer_error("second".to_string());
        let error = emitter.encode().expect_err("deferred");
        assert!(error.to_string().contains("first"));
    }

    #[test]
    fn all_operations_are_callable_inside_a_frame() {
        let emitter = Emitter::new(0);
        emitter
            .block("outer", BlockType::Empty, || {
                emitter.emit(Instruction::Nop);
                emitter.local(ValType::I32);
                emitter.loop_("inner1", BlockType::Empty, || Ok(()))?;
                emitter.loop_("inner2", BlockType::Empty, || {
                    emitter.emit(Instruction::I32Const(1));
                    emitter.if_(BlockType::Empty, || Ok(()))?;
                    Ok(())
                })?;
                emitter.block("inner3", BlockType::Empty, || Ok(()))?;
                emitter.emit(Instruction::I32Const(1));
                emitter
                    .if_(BlockType::Result(ValType::I32), || {
                        emitter.emit(Instruction::I32Const(0));
                        Ok(())
                    })?
                    .else_(|| {
                        emitter.emit(Instruction::I32Const(1));
                        Ok(())
                    })?;
                emitter.br_if("outer")?;
                emitter.emit(Instruction::Nop);
                emitter.emit(Instruction::I32Const(0));
                emitter.br_table(&["outer"], "outer")?;
                emitter.trap();
                emitter.br("outer")
            })
            .expect("no re-entrancy panic");
        validate(&emitter, vec![], vec![]);
    }
}
