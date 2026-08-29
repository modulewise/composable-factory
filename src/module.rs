//! The core Wasm module a component wraps: its sections and their encoding.
//! Has no direct knowledge of WIT or the Canonical ABI.

use wasm_encoder::{
    CodeSection, ConstExpr, DataSection, EntityType, ExportKind, ExportSection,
    Function as EncodedFunction, FunctionSection, GlobalSection, GlobalType, ImportSection,
    MemorySection, MemoryType, Module, TypeSection, ValType,
};

/// A core module ready to encode: what it imports, what it defines, and the
/// memory it operates on.
pub struct CoreModule {
    pub imports: Vec<CoreImport>,
    pub functions: Vec<CoreFunction>,
    pub memories: Vec<CoreMemory>,
    pub globals: Vec<CoreGlobal>,
    pub types: TypeTable,
    pub data: Strings,
}

impl CoreModule {
    /// Encode to core Wasm bytes.
    pub fn encode(mut self) -> Vec<u8> {
        let mut imports = ImportSection::new();
        for import in &self.imports {
            let type_index = self.types.func_type(&import.params, &import.results);
            imports.import(
                &import.module,
                &import.name,
                EntityType::Function(type_index),
            );
        }

        let mut functions = FunctionSection::new();
        for function in &self.functions {
            let type_index = self.types.func_type(&function.params, &function.results);
            functions.function(type_index);
        }

        let mut memories = MemorySection::new();
        for memory in &self.memories {
            memories.memory(memory.ty);
        }

        let mut globals = GlobalSection::new();
        for global in &self.globals {
            globals.global(global.ty, &global.init);
        }

        // Imports occupy the function index space first, so definitions follow them.
        let mut exports = ExportSection::new();
        let first_defined = self.imports.len() as u32;
        for (offset, function) in self.functions.iter().enumerate() {
            exports.export(
                &function.export_name,
                ExportKind::Func,
                first_defined + offset as u32,
            );
        }
        for (index, memory) in self.memories.iter().enumerate() {
            if let Some(name) = &memory.export_name {
                exports.export(name, ExportKind::Memory, index as u32);
            }
        }

        let mut code = CodeSection::new();
        for function in &self.functions {
            code.function(&function.body);
        }

        let mut data = DataSection::new();
        if !self.data.is_empty() {
            data.active(
                0,
                &ConstExpr::i32_const(0),
                self.data.bytes().iter().copied(),
            );
        }

        let mut types = TypeSection::new();
        for (params, results) in self.types.func_types() {
            types
                .ty()
                .function(params.iter().copied(), results.iter().copied());
        }

        let mut module = Module::new();
        module
            .section(&types)
            .section(&imports)
            .section(&functions)
            .section(&memories)
            .section(&globals)
            .section(&exports)
            .section(&code)
            .section(&data);
        module.finish()
    }
}

/// One import the module declares.
pub struct CoreImport {
    pub module: String,
    pub name: String,
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

/// One function the module defines: its signature, its body, and the name it
/// is exported under.
pub struct CoreFunction {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
    pub body: EncodedFunction,
    pub export_name: String,
}

/// One memory the module declares, and the name it is exported under.
pub struct CoreMemory {
    pub ty: MemoryType,
    pub export_name: Option<String>,
}

/// One global the module declares.
pub struct CoreGlobal {
    pub ty: GlobalType,
    pub init: ConstExpr,
}

/// The func types the module declares, interned so identical signatures share
/// one index.
#[derive(Default)]
pub struct TypeTable {
    func_types: Vec<(Vec<ValType>, Vec<ValType>)>,
}

impl TypeTable {
    /// Intern a func type and return its index in the type section.
    pub fn func_type(&mut self, params: &[ValType], results: &[ValType]) -> u32 {
        if let Some(index) = self
            .func_types
            .iter()
            .position(|(p, r)| p == params && r == results)
        {
            return index as u32;
        }
        self.func_types.push((params.to_vec(), results.to_vec()));
        (self.func_types.len() - 1) as u32
    }

    /// The interned types in index order.
    fn func_types(&self) -> &[(Vec<ValType>, Vec<ValType>)] {
        &self.func_types
    }
}

/// The interned strings that become the module's data segment at address 0.
#[derive(Default)]
pub struct Strings {
    buf: String,
    offsets: std::collections::HashMap<String, (u32, u32)>,
}

impl Strings {
    /// Intern `s` and return its `(offset, len)` within the data segment.
    /// Identical strings share one entry.
    pub fn intern(&mut self, s: &str) -> (u32, u32) {
        if let Some(&entry) = self.offsets.get(s) {
            return entry;
        }
        let entry = (self.buf.len() as u32, s.len() as u32);
        self.buf.push_str(s);
        self.offsets.insert(s.to_string(), entry);
        entry
    }

    /// The interned bytes in offset order.
    fn bytes(&self) -> &[u8] {
        self.buf.as_bytes()
    }

    /// Whether anything has been interned.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The total interned length (where the data segment ends).
    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

/// Round up to the next multiple of 8, the smallest granularity that satisfies
/// every wasm32 alignment.
pub fn align8(n: usize) -> usize {
    (n + 7) & !7
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::Instruction;

    fn empty() -> CoreModule {
        CoreModule {
            imports: Vec::new(),
            functions: Vec::new(),
            memories: Vec::new(),
            globals: Vec::new(),
            types: TypeTable::default(),
            data: Strings::default(),
        }
    }

    fn memory() -> CoreMemory {
        CoreMemory {
            export_name: None,
            ty: MemoryType {
                minimum: 1,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            },
        }
    }

    // A function with no params or results whose body is `instructions` then `end`.
    fn function(export_name: &str, instructions: &[Instruction]) -> CoreFunction {
        let mut body = EncodedFunction::new(std::iter::empty::<(u32, ValType)>());
        for instruction in instructions {
            body.instruction(instruction);
        }
        body.instruction(&Instruction::End);
        CoreFunction {
            params: Vec::new(),
            results: Vec::new(),
            body,
            export_name: export_name.to_string(),
        }
    }

    fn validate(module: CoreModule) -> Vec<u8> {
        let bytes = module.encode();
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::all())
            .validate_all(&bytes)
            .expect("the encoded module must be valid wasm");
        bytes
    }

    #[test]
    fn empty_module_is_valid() {
        validate(empty());
    }

    #[test]
    fn imports_precede_definitions_in_the_function_index_space() {
        let mut module = empty();
        module.imports.push(CoreImport {
            module: "logger".to_string(),
            name: "log".to_string(),
            params: Vec::new(),
            results: Vec::new(),
        });
        module.functions.push(function("run", &[]));
        let bytes = validate(module);

        // The exported function definition is at index 1, after the import.
        let exported = read_exports(&bytes);
        assert_eq!(exported, vec![("run".to_string(), 1)]);
    }

    #[test]
    fn a_body_can_call_an_import_by_its_index() {
        let mut module = empty();
        module.imports.push(CoreImport {
            module: "logger".to_string(),
            name: "log".to_string(),
            params: vec![ValType::I32],
            results: Vec::new(),
        });
        module.functions.push(function(
            "run",
            &[Instruction::I32Const(1), Instruction::Call(0)],
        ));
        // The import takes an i32 but `run` takes nothing, so calling index 0
        // type-checks against the import and nothing else in this module.
        validate(module);
    }

    #[test]
    fn identical_signatures_share_one_type() {
        let mut module = empty();
        for name in ["first", "second"] {
            module.functions.push(function(name, &[]));
        }
        let bytes = validate(module);
        assert_eq!(read_type_count(&bytes), 1, "both functions are `() -> ()`");
    }

    #[test]
    fn differing_signatures_get_their_own_types() {
        let mut module = empty();
        module.functions.push(function("empty", &[]));
        let mut returns_one = function("one", &[Instruction::I32Const(1)]);
        returns_one.results.push(ValType::I32);
        module.functions.push(returns_one);
        let bytes = validate(module);
        assert_eq!(read_type_count(&bytes), 2);
    }

    #[test]
    fn identical_strings_share_one_offset() {
        let mut data = Strings::default();
        assert_eq!(data.intern("hello"), (0, 5));
        assert_eq!(data.intern("world"), (5, 5));
        assert_eq!(data.intern("hello"), (0, 5));
        assert_eq!(data.len(), 10);
    }

    #[test]
    fn interned_strings_are_the_data_segment() {
        let mut module = empty();
        module.memories.push(memory());
        module.data.intern("hello");
        let bytes = validate(module);
        assert_eq!(read_data_segments(&bytes), vec![(0u64, b"hello".to_vec())]);
    }

    #[test]
    fn a_module_with_no_strings_has_no_data_segment() {
        let mut module = empty();
        module.memories.push(memory());
        let bytes = validate(module);
        assert!(read_data_segments(&bytes).is_empty());
    }

    #[test]
    fn a_mutable_global_is_declared_with_its_initial_value() {
        let mut module = empty();
        module.globals.push(CoreGlobal {
            ty: GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            init: ConstExpr::i32_const(64),
        });
        // A body that reads and writes it only validates if it is mutable.
        module.functions.push(function(
            "increment",
            &[
                Instruction::GlobalGet(0),
                Instruction::I32Const(1),
                Instruction::I32Add,
                Instruction::GlobalSet(0),
            ],
        ));
        validate(module);
    }

    #[test]
    fn align8_rounds_up_to_the_next_multiple() {
        assert_eq!(align8(0), 0);
        assert_eq!(align8(1), 8);
        assert_eq!(align8(7), 8);
        assert_eq!(align8(8), 8);
        assert_eq!(align8(9), 16);
    }

    // Read the type count from encoded bytes.
    fn read_type_count(bytes: &[u8]) -> usize {
        for payload in wasmparser::Parser::new(0).parse_all(bytes).flatten() {
            if let wasmparser::Payload::TypeSection(reader) = payload {
                return reader.count() as usize;
            }
        }
        0
    }

    // Read the export names and indices from encoded bytes.
    fn read_exports(bytes: &[u8]) -> Vec<(String, u32)> {
        let mut found = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(bytes).flatten() {
            if let wasmparser::Payload::ExportSection(reader) = payload {
                for export in reader.into_iter().flatten() {
                    found.push((export.name.to_string(), export.index));
                }
            }
        }
        found
    }

    // Read the data segments from encoded bytes.
    fn read_data_segments(bytes: &[u8]) -> Vec<(u64, Vec<u8>)> {
        let mut found = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(bytes).flatten() {
            if let wasmparser::Payload::DataSection(reader) = payload {
                for segment in reader.into_iter().flatten() {
                    let offset = match segment.kind {
                        wasmparser::DataKind::Active { offset_expr, .. } => {
                            let mut ops = offset_expr.get_operators_reader();
                            match ops.read() {
                                Ok(wasmparser::Operator::I32Const { value }) => value as u64,
                                other => panic!("expected an i32 offset, got {other:?}"),
                            }
                        }
                        wasmparser::DataKind::Passive => panic!("expected an active segment"),
                    };
                    found.push((offset, segment.data.to_vec()));
                }
            }
        }
        found
    }
}
