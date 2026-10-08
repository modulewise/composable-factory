//! The canonical ABI, checked against wasmtime as the reference.
//!
//! For each type, a generated component moves values of it across every kind
//! of boundary the factory emits code for: export params and results (flat,
//! through memory, and indirect past the flat limits), import results and
//! args, `task.return`, members of records, payloads of variants, list
//! elements, and member-by-member rebuilds. The host sends a value in and
//! checks what comes back, so any disagreement with wasmtime's lifting and
//! lowering is a failure, whatever the generated code looks like.
//!
//! Every instance is reused across calls, and most calls are preceded by one
//! that leaves random bytes of a random size in the heap, so code that reads
//! bytes it did not write, or writes past what it owns, sees garbage rather
//! than the zeroes of a fresh instance.
//!
//! Every limit is tested on both sides, by [`boundary_types`] on every run,
//! and a share of every random run is past one ([`TypeGen::boundary`]).
//!
//! The Source column names definitions in the canonical ABI spec
//! (`CanonicalABI.md` and its reference implementation,
//! `canonical-abi/definitions.py`), in
//! https://github.com/WebAssembly/component-model/tree/main/design/mvp,
//! unless it names another crate.
//!
//! | Limit | Source | Boundary types |
//! |---|---|---|
//! | `MAX_FLAT_PARAMS` = 16: export params, and `task.return` | `MAX_FLAT_PARAMS` | `p16`, `p17`; separate params in `params.rs` |
//! | `MAX_FLAT_PARAMS` = 16: import args (always lowered sync here, so `MAX_FLAT_ASYNC_PARAMS` = 4 does not apply) | as above | `p16`, `p17` (`forward`); `params.rs` |
//! | `MAX_FLAT_RESULTS` = 1: sync results | `MAX_FLAT_RESULTS` | `u32`, `tuple<u32, u32>` |
//! | 64 flats: the most `abi::flat_types` flattens (its buffer) | this crate's `abi::flat_types` | `x64`, `x65`, nested `list<tuple<w, u32>, 4>` |
//! | discriminant `u8` / `u16` at 256 cases; `u32` (past 65536) is unreachable, since wasmparser accepts at most 10,000 enum or variant cases | `discriminant_type`; wasmparser's `MAX_WASM_ENUM_CASES`, `MAX_WASM_VARIANT_CASES` | `e256`, `e257`, `v256`, `v257`, `e10000` |
//! | flags `u8` / `u16` / `u32` at 8 and 16 flags, 32 at most | `alignment_flags` | `g8`, `g9`, `g16`, `g17`, `g32` |
//! | joined payloads: `i32`, `f32`, `i64`, `f64` | `join` | `j1`..`j7` |
//! | payloads aligned past a 1-byte discriminant | `alignment_variant` | `option<u64>`, `result<f64, u8>`, `a1` |
//! | nesting depth | (not a spec limit) | `deep10` |
//!
//! A plain `cargo test` skips the random types, which take about 2 minutes
//! per seed; `tests/abi/run.sh` runs the whole suite. `ABI_SEEDS`
//! (comma-separated) and `ABI_CASES` set the random runs' seeds and types
//! per seed. Each run reports which limits its types crossed.

mod generate;
mod harness;
mod narrow;
mod params;
#[path = "../support/mod.rs"]
mod support;

use anyhow::Result;
use generate::{Rng, Ty, TypeGen};
use harness::Run;

/// Types chosen for the layouts that differ between memory and flat form.
fn fixed_types() -> Vec<Ty> {
    use Ty::*;
    let named = |prefix: &str, i: usize| format!("{prefix}{i}");
    let record = |i: usize, fields: Vec<Ty>| {
        Record(
            named("k", i),
            fields
                .into_iter()
                .enumerate()
                .map(|(n, t)| (format!("m{n}"), t))
                .collect(),
        )
    };
    let tool_result = record(
        4,
        vec![
            List(Box::new(String)),
            Bool,
            Option(Box::new(String)),
            List(Box::new(Tuple(vec![String, String]))),
        ],
    );
    vec![
        Bool,
        U8,
        U16,
        Char,
        F32,
        F64,
        U64,
        String,
        record(1, vec![Bool, Bool]),
        record(2, vec![U8, U8]),
        record(3, vec![U8, U16, U8, U64, Bool]),
        Result(Some(Box::new(U64)), Some(Box::new(U8))),
        Result(
            Some(Box::new(tool_result.clone())),
            Some(Box::new(record(
                5,
                vec![S32, String, Option(Box::new(String))],
            ))),
        ),
        Option(Box::new(U8)),
        Option(Box::new(Option(Box::new(Bool)))),
        Option(Box::new(F64)),
        Variant(
            named("v", 1),
            vec![
                ("a".into(), Some(F32)),
                ("b".into(), Some(U64)),
                ("c".into(), None),
                ("d".into(), Some(U8)),
            ],
        ),
        Enum(named("e", 1), (0..300).map(|i| format!("e{i}")).collect()),
        Flags(named("g", 1), (0..9).map(|i| format!("g{i}")).collect()),
        Flags(named("g", 2), (0..17).map(|i| format!("g{i}")).collect()),
        Flags(named("g", 3), (0..32).map(|i| format!("g{i}")).collect()),
        List(Box::new(record(6, vec![U8, U16]))),
        List(Box::new(Option(Box::new(U8)))),
        Tuple(vec![U8, U64, Bool]),
        record(7, (0..17).map(|_| Bool).collect()),
        record(8, (0..9).map(|_| String).collect()),
        FixedList(Box::new(U8), 3),
        Map(Box::new(String), Box::new(U8)),
    ]
}

/// Types on both sides of every limit in the table above.
fn boundary_types() -> Vec<Ty> {
    use Ty::*;
    let fields = |types: Vec<Ty>| -> Vec<(std::string::String, Ty)> {
        types
            .into_iter()
            .enumerate()
            .map(|(n, t)| (format!("m{n}"), t))
            .collect()
    };
    let narrow = |name: &str, n: usize| Record(name.into(), fields(vec![U8; n]));
    let cases = |prefix: &str, n: usize| (0..n).map(|i| format!("{prefix}{i}")).collect();
    let variant = |name: &str, n: usize| {
        // Every third case carries a payload, of alternating width.
        let cases = (0..n)
            .map(|i| {
                let payload = match i % 6 {
                    0 => Some(U8),
                    3 => Some(U64),
                    _ => None,
                };
                (format!("c{i}"), payload)
            })
            .collect();
        Variant(name.into(), cases)
    };
    let join = |name: &str, payloads: Vec<Ty>| {
        let cases = payloads
            .into_iter()
            .enumerate()
            .map(|(i, t)| (format!("c{i}"), Some(t)))
            .collect();
        Variant(name.into(), cases)
    };
    let wide = Record(
        "w".into(),
        fields(
            (0..24)
                .map(|i| [Bool, U8, U16, S16, U64][i % 5].clone())
                .collect(),
        ),
    );
    let mut deep = Bool;
    for level in 0..10 {
        deep = match level % 5 {
            0 => Option(Box::new(deep)),
            1 => Record(format!("d{level}"), fields(vec![deep, U8])),
            2 => List(Box::new(deep)),
            3 => Variant(
                format!("d{level}"),
                vec![("c0".into(), Some(deep)), ("c1".into(), None)],
            ),
            _ => Result(Some(Box::new(deep)), Some(Box::new(U16))),
        };
    }
    vec![
        narrow("p16", 16),
        narrow("p17", 17),
        U32,
        Tuple(vec![U32, U32]),
        narrow("x64", 64),
        narrow("x65", 65),
        FixedList(Box::new(Tuple(vec![wide, U32])), 4),
        Enum("e256".into(), cases("e", 256)),
        Enum("e257".into(), cases("e", 257)),
        variant("v256", 256),
        variant("v257", 257),
        Enum("e10000".into(), cases("e", 10_000)),
        Flags("g8".into(), cases("g", 8)),
        Flags("g9".into(), cases("g", 9)),
        Flags("g16".into(), cases("g", 16)),
        Flags("g17".into(), cases("g", 17)),
        Flags("g32".into(), cases("g", 32)),
        join("j1", vec![U32, F32]),
        join("j2", vec![F32, F64]),
        join("j3", vec![U64, F64]),
        join("j4", vec![F32, U64]),
        join("j5", vec![U8, F64]),
        join("j6", vec![String, F64]),
        join("j7", vec![String, U64, F32]),
        Option(Box::new(U64)),
        Result(Some(Box::new(F64)), Some(Box::new(U8))),
        Record("a1".into(), fields(vec![U8, U64, U8, U16, U8, F64])),
        deep,
    ]
}

#[test]
fn fixed_types_round_trip() -> Result<()> {
    let mut run = Run::new(1)?;
    for ty in fixed_types() {
        run.check(&ty)?;
    }
    run.finish()
}

#[test]
fn boundary_types_round_trip() -> Result<()> {
    let mut run = Run::new(2)?;
    for ty in boundary_types() {
        run.check(&ty)?;
    }
    run.finish()
}

#[test]
#[ignore = "slow: about 2 minutes per seed; run with tests/abi/run.sh"]
fn random_types_round_trip() -> Result<()> {
    let seeds: Vec<u64> = std::env::var("ABI_SEEDS")
        .ok()
        .map(|seeds| {
            seeds
                .split(',')
                .filter_map(|s| s.trim().parse().ok())
                .collect()
        })
        .unwrap_or_else(|| vec![0x5EED]);
    let cases = std::env::var("ABI_CASES")
        .ok()
        .and_then(|cases| cases.parse().ok())
        .unwrap_or(150);
    let mut failed = Vec::new();
    for seed in seeds {
        eprintln!("ABI_SEEDS={seed} ABI_CASES={cases}");
        let mut types = Rng::new(seed);
        let mut run = Run::new(seed)?;
        for case in 0..cases {
            // Every fourth type is past a limit, so each run crosses them.
            let ty = if case % 4 == 0 {
                TypeGen::new(&mut types).boundary()
            } else {
                TypeGen::new(&mut types).ty(3)
            };
            run.check(&ty)?;
        }
        if let Err(e) = run.finish() {
            failed.push(format!("seed {seed}: {e}"));
        }
    }
    anyhow::ensure!(failed.is_empty(), "{}", failed.join("; "));
    Ok(())
}
