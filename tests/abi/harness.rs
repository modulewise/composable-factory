//! Builds a component per type, calls each crossing on a reused instance with
//! dirty memory, and compares what comes back with what was sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, anyhow, bail};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, Kind, MatchArm, Value, ValueSpec, arm};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Func, Instance, Linker, Val};
use wasmtime::{Config, Engine, Store};

use crate::generate::{MAX_ELEMENTS, Rng, Ty, ValGen};
use crate::support::{block_on, join};

/// The literals written around a value in `wrap`.
const PRE: u8 = 0xA5;
const POST: u16 = 0xBEEF;

/// Calls per instance, beyond one of each crossing.
const EXTRA_CALLS: usize = 12;

/// The largest heap fill before a call.
const MAX_FILL: usize = 300_000;

/// One way a value crosses a boundary in generated code.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Crossing {
    /// Export param to export result.
    Id,
    /// Export param, between narrow params, to export result.
    IdBetween,
    /// Export param to `task.return`.
    IdAsync,
    /// Import result to export result.
    FromImport,
    /// Async import result to `task.return`.
    FromImportAsync,
    /// Export param to import arg.
    Forward,
    /// Import result to import arg.
    ForwardImport,
    /// Export param into a record, between narrow literals.
    Wrap,
    /// Import result into a record, between narrow literals.
    WrapImport,
    /// Export param into `option`'s payload.
    Some,
    /// Async import result into `option`'s payload, to `task.return`.
    SomeImportAsync,
    /// Export param into `result`'s ok payload.
    Ok,
    /// Export param into `result`'s err payload.
    Err,
    /// Export param into two list elements.
    Twice,
    /// Export param, rebuilt member by member, to export result.
    Deep,
    /// Export param, rebuilt member by member, to `task.return`.
    DeepAsync,
    /// Import result, rebuilt member by member, to export result.
    DeepImport,
}

const CROSSINGS: [Crossing; 17] = [
    Crossing::Id,
    Crossing::IdBetween,
    Crossing::IdAsync,
    Crossing::FromImport,
    Crossing::FromImportAsync,
    Crossing::Forward,
    Crossing::ForwardImport,
    Crossing::Wrap,
    Crossing::WrapImport,
    Crossing::Some,
    Crossing::SomeImportAsync,
    Crossing::Ok,
    Crossing::Err,
    Crossing::Twice,
    Crossing::Deep,
    Crossing::DeepAsync,
    Crossing::DeepImport,
];

impl Crossing {
    fn name(self) -> &'static str {
        match self {
            Crossing::Id => "id",
            Crossing::IdBetween => "id-between",
            Crossing::IdAsync => "id-async",
            Crossing::FromImport => "from-import",
            Crossing::FromImportAsync => "from-import-async",
            Crossing::Forward => "forward",
            Crossing::ForwardImport => "forward-import",
            Crossing::Wrap => "wrap",
            Crossing::WrapImport => "wrap-import",
            Crossing::Some => "some",
            Crossing::SomeImportAsync => "some-import-async",
            Crossing::Ok => "ok",
            Crossing::Err => "err",
            Crossing::Twice => "twice",
            Crossing::Deep => "deep",
            Crossing::DeepAsync => "deep-async",
            Crossing::DeepImport => "deep-import",
        }
    }

    fn signature(self) -> &'static str {
        match self {
            Crossing::Id | Crossing::Deep => "func(v: t) -> t",
            Crossing::IdBetween => "func(a: u8, v: t, b: bool) -> t",
            Crossing::IdAsync | Crossing::DeepAsync => "async func(v: t) -> t",
            Crossing::FromImport | Crossing::DeepImport => "func() -> t",
            Crossing::FromImportAsync => "async func() -> t",
            Crossing::Forward => "func(v: t)",
            Crossing::ForwardImport => "func()",
            Crossing::Wrap => "func(v: t) -> wrap",
            Crossing::WrapImport => "func() -> wrap",
            Crossing::Some => "func(v: t) -> option<t>",
            Crossing::SomeImportAsync => "async func() -> option<t>",
            Crossing::Ok => "func(v: t) -> result<t, u8>",
            Crossing::Err => "func(v: t) -> result<u8, t>",
            Crossing::Twice => "func(v: t) -> list<t>",
        }
    }

    /// Whether the value comes from the `src` import rather than a param.
    fn sourced_from_import(self) -> bool {
        matches!(
            self,
            Crossing::FromImport
                | Crossing::FromImportAsync
                | Crossing::ForwardImport
                | Crossing::WrapImport
                | Crossing::SomeImportAsync
                | Crossing::DeepImport
        )
    }

    /// Whether the value reaches the host through the `sink` import.
    fn delivered_to_import(self) -> bool {
        matches!(self, Crossing::Forward | Crossing::ForwardImport)
    }

    fn args(self, v: &Val) -> Vec<Val> {
        match self {
            Crossing::IdBetween => vec![Val::U8(0xC3), v.clone(), Val::Bool(true)],
            _ if self.sourced_from_import() => vec![],
            _ => vec![v.clone()],
        }
    }

    /// What the host should get back for `v`: the call's result, or what the
    /// `sink` import received, for a crossing that delivers to it.
    fn expected(self, v: &Val) -> Val {
        let boxed = || Some(Box::new(v.clone()));
        match self {
            Crossing::Wrap | Crossing::WrapImport => Val::Record(vec![
                ("pre".into(), Val::U8(PRE)),
                ("v".into(), v.clone()),
                ("post".into(), Val::U16(POST)),
            ]),
            Crossing::Some | Crossing::SomeImportAsync => Val::Option(boxed()),
            Crossing::Ok => Val::Result(Ok(boxed())),
            Crossing::Err => Val::Result(Err(boxed())),
            Crossing::Twice => Val::List(vec![v.clone(), v.clone()]),
            _ => v.clone(),
        }
    }
}

/// The WIT for a component over `ty` with `crossings` (and `fill`) exported.
fn wit(ty: &Ty, crossings: &[Crossing]) -> String {
    let mut definitions = Vec::new();
    ty.definitions(&mut definitions);
    let exports: String = crossings
        .iter()
        .map(|c| format!("  export {}: {};\n", c.name(), c.signature()))
        .collect();
    format!(
        "package test:abi;
interface types {{
  {}
  type t = {};
  record wrap {{ pre: u8, v: t, post: u16 }}
}}
interface src {{
  use types.{{t}};
  get: func() -> t;
  get-async: async func() -> t;
}}
interface sink {{
  use types.{{t}};
  put: func(v: t);
}}
world abi {{
  use types.{{t, wrap}};
  import src;
  import sink;
  export fill: func(data: list<u8>);
{exports}}}
",
        definitions.join("\n  "),
        ty.wit(),
    )
}

struct Builder {
    wit: String,
}

impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let abi = PackageSource::from_text(&self.wit)?.world("abi")?;
        world.add_imports(abi.imports())?;
        world.add_exports(abi.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        if function.name() == "fill" {
            return Ok(());
        }
        let crossing = CROSSINGS
            .iter()
            .copied()
            .find(|c| c.name() == function.name())
            .ok_or_else(|| anyhow!("unknown export {}", function.name()))?;
        let src = imports.interface("src")?;
        let v = if crossing.sourced_from_import() {
            let get = if matches!(
                crossing,
                Crossing::FromImportAsync | Crossing::SomeImportAsync
            ) {
                "get-async"
            } else {
                "get"
            };
            src.function(get)?.call(&[])?.context("get returns t")?
        } else {
            function.param("v")?.receive()?
        };
        if crossing.delivered_to_import() {
            imports.interface("sink")?.function("put")?.call(&[v])?;
            return Ok(());
        }
        let result = function.result().context("returns")?.value();
        match crossing {
            Crossing::Wrap | Crossing::WrapImport => result.write(&ValueSpec::record([
                ("pre", ValueSpec::u8(PRE)),
                ("v", ValueSpec::from(&v)),
                ("post", ValueSpec::u16(POST)),
            ])),
            Crossing::Some | Crossing::SomeImportAsync => {
                result.write(&ValueSpec::some(ValueSpec::from(&v)))
            }
            Crossing::Ok => result.write(&ValueSpec::ok(ValueSpec::from(&v))),
            Crossing::Err => result.write(&ValueSpec::err(ValueSpec::from(&v))),
            Crossing::Twice => {
                result.write(&ValueSpec::list([ValueSpec::from(&v), ValueSpec::from(&v)]))
            }
            Crossing::Deep | Crossing::DeepAsync | Crossing::DeepImport => write_deep(&result, &v),
            _ => result.write(&ValueSpec::from(&v)),
        }
    }
}

/// Write `src` into `dest` member by member: records field by field, lists
/// element by element, and variant-likes case by case. Anything else is
/// copied whole.
fn write_deep(dest: &Value, src: &Value) -> Result<()> {
    let (kind, cases): (VariantLike, Vec<String>) = match src.ty().kind() {
        Kind::Option(_) => (VariantLike::Option, vec!["none".into(), "some".into()]),
        Kind::Result { .. } => (VariantLike::Result, vec!["ok".into(), "err".into()]),
        Kind::Variant(cases) => (
            VariantLike::Variant,
            cases.iter().map(|c| c.name().to_string()).collect(),
        ),
        _ => return dest.write(&spec_of(src)?),
    };
    let arms: Vec<MatchArm<'_>> = cases
        .into_iter()
        .map(|case| {
            let dest = dest.clone();
            arm(case.clone(), move |payload| {
                let payload = payload.map(|p| spec_of(&p)).transpose()?;
                dest.write(&case_spec(kind, &case, payload))
            })
        })
        .collect();
    src.dispatch(arms)
}

/// A spec for `v` that rebuilds records and lists from their members.
fn spec_of(v: &Value) -> Result<ValueSpec> {
    match v.ty().kind() {
        Kind::Record(fields) => {
            let fields = fields
                .iter()
                .map(|field| Ok((field.name().to_string(), spec_of(&v.field(field.name())?)?)))
                .collect::<Result<Vec<_>>>()?;
            Ok(ValueSpec::record(fields))
        }
        Kind::List(_) => Ok(ValueSpec::from(v.map(v.ty(), |element| spec_of(&element))?)),
        _ => Ok(ValueSpec::from(v)),
    }
}

/// Which kind of variant-like value a dispatched value is.
#[derive(Clone, Copy)]
enum VariantLike {
    Option,
    Result,
    Variant,
}

/// The spec for one case of a variant-like.
fn case_spec(kind: VariantLike, case: &str, payload: Option<ValueSpec>) -> ValueSpec {
    match (kind, payload) {
        (VariantLike::Option, Some(payload)) => ValueSpec::some(payload),
        (VariantLike::Option, None) => ValueSpec::none(),
        (VariantLike::Result, Some(payload)) if case == "ok" => ValueSpec::ok(payload),
        (VariantLike::Result, Some(payload)) => ValueSpec::err(payload),
        (_, Some(payload)) => ValueSpec::variant(case, payload),
        (_, None) => ValueSpec::variant_unit(case),
    }
}

/// What the host imports hold: the value `get` returns, what `put`
/// received, and whether `get-async` blocks before returning.
#[derive(Default)]
struct Host {
    next: Option<Val>,
    received: Option<Val>,
    block: bool,
}

/// One failure, with what is needed to reproduce it.
struct Failure {
    crossing: String,
    ty: String,
    detail: String,
}

/// Checks types, collecting every failure rather than stopping at the first.
pub struct Run {
    engine: Engine,
    rng: Rng,
    failures: Vec<Failure>,
    calls: usize,
    types: usize,
    /// How many of the types checked crossed each limit, by label.
    crossed: Vec<(&'static str, usize)>,
}

/// A limit's label, and a test of whether a type crosses it.
type Limit = (&'static str, fn(&Ty) -> bool);

/// The limits a type can cross. Reported by every run, so a run shows which
/// limits it exercised.
const LIMITS: [Limit; 8] = [
    ("> 16 flats (indirect params, task.return)", |ty| {
        ty.flats() > 16
    }),
    ("> 64 flats (cannot be flattened)", |ty| ty.flats() > 64),
    ("> 1 flat (indirect sync result)", |ty| ty.flats() > 1),
    ("> 256 cases (u16 discriminant)", |ty| ty.most_cases() > 256),
    ("> 8 flags (u16 flags)", |ty| ty.most_flags() > 8),
    ("> 16 flags (u32 flags)", |ty| ty.most_flags() > 16),
    ("depth >= 5", |ty| ty.depth() >= 5),
    ("depth >= 10", |ty| ty.depth() >= 10),
];

impl Run {
    pub fn new(seed: u64) -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model_async(true);
        config.wasm_component_model_async_stackful(true);
        config.wasm_component_model_map(true);
        config.wasm_component_model_fixed_length_lists(true);
        Ok(Run {
            engine: Engine::new(&config)?,
            rng: Rng::new(seed.wrapping_add(1)),
            failures: Vec::new(),
            calls: 0,
            types: 0,
            crossed: LIMITS.iter().map(|(label, _)| (*label, 0)).collect(),
        })
    }

    fn fail(&mut self, crossing: &str, ty: &Ty, detail: String) {
        self.failures.push(Failure {
            crossing: crossing.to_string(),
            ty: describe(ty),
            detail,
        });
    }

    /// Check every crossing for `ty`. A component that fails to build is
    /// rebuilt with one crossing at a time, so each failure is attributed.
    pub fn check(&mut self, ty: &Ty) -> Result<()> {
        self.types += 1;
        for ((_, crosses), (_, count)) in LIMITS.iter().zip(&mut self.crossed) {
            if crosses(ty) {
                *count += 1;
            }
        }
        match self.component(ty, &CROSSINGS) {
            Ok(component) => self.exercise(ty, &component, &CROSSINGS),
            Err(_) => {
                for crossing in CROSSINGS {
                    match self.component(ty, &[crossing]) {
                        Ok(component) => self.exercise(ty, &component, &[crossing])?,
                        Err(e) => self.fail(crossing.name(), ty, format!("build: {e:#}")),
                    }
                }
                Ok(())
            }
        }
    }

    fn component(&self, ty: &Ty, crossings: &[Crossing]) -> Result<Component> {
        let bytes = build(&Builder {
            wit: wit(ty, crossings),
        })?;
        Ok(Component::new(&self.engine, &bytes)?)
    }

    /// Call each crossing twice in random order, then more at random, on one
    /// instance, most calls preceded by a heap fill; then two async calls at
    /// once.
    fn exercise(&mut self, ty: &Ty, component: &Component, crossings: &[Crossing]) -> Result<()> {
        let host = Arc::new(Mutex::new(Host::default()));
        let linker = linker(&self.engine, &host)?;
        let mut order: Vec<Crossing> = crossings.iter().chain(crossings).copied().collect();
        for i in (1..order.len()).rev() {
            order.swap(i, self.rng.below(i as u64 + 1) as usize);
        }
        for _ in 0..EXTRA_CALLS {
            order.push(*self.rng.pick(crossings));
        }
        block_on(async {
            let mut store = new_store(&self.engine);
            let mut instance = linker.instantiate_async(&mut store, component).await?;
            for crossing in order {
                if self.rng.chance(75) {
                    let size = self.rng.below(MAX_FILL as u64) as usize;
                    let fill = self.fill(size);
                    if let Err(e) = call(&mut store, &instance, "fill", vec![fill], 0).await {
                        self.fail("fill", ty, format!("{e:#}"));
                        // A trap leaves the instance unusable.
                        store = new_store(&self.engine);
                        instance = linker.instantiate_async(&mut store, component).await?;
                    }
                }
                let v = ValGen::new(&mut self.rng).val(ty);
                {
                    let mut host = host.lock().unwrap();
                    host.next = Some(v.clone());
                    host.received = None;
                    host.block = self.rng.chance(50);
                }
                self.calls += 1;
                let results = usize::from(!crossing.delivered_to_import());
                match call(
                    &mut store,
                    &instance,
                    crossing.name(),
                    crossing.args(&v),
                    results,
                )
                .await
                {
                    Err(e) => {
                        self.fail(
                            crossing.name(),
                            ty,
                            format!("call: {e:#}\n    value: {}", show(&v)),
                        );
                        store = new_store(&self.engine);
                        instance = linker.instantiate_async(&mut store, component).await?;
                    }
                    Ok(got) => {
                        let got = if crossing.delivered_to_import() {
                            host.lock().unwrap().received.take()
                        } else {
                            got.into_iter().next()
                        };
                        let expected = crossing.expected(&v);
                        if got.as_ref() != Some(&expected) {
                            self.fail(
                                crossing.name(),
                                ty,
                                format!(
                                    "mismatch\n    sent: {}\n    got:  {}",
                                    show(&v),
                                    got.as_ref().map_or("nothing".into(), show)
                                ),
                            );
                        }
                    }
                }
            }
            if crossings.contains(&Crossing::IdAsync) && crossings.contains(&Crossing::DeepAsync) {
                self.concurrently(ty, &mut store, &instance, &host).await;
            }
            anyhow::Ok(())
        })
    }

    /// `id-async` and `deep-async` in progress at once, after a fill.
    async fn concurrently(
        &mut self,
        ty: &Ty,
        store: &mut Store<()>,
        instance: &Instance,
        host: &Arc<Mutex<Host>>,
    ) {
        let size = self.rng.below(MAX_FILL as u64) as usize;
        let fill = self.fill(size);
        let _ = call(store, instance, "fill", vec![fill], 0).await;
        let a = ValGen::new(&mut self.rng).val(ty);
        let b = ValGen::new(&mut self.rng).val(ty);
        host.lock().unwrap().block = true;
        let outcome = async {
            let first = func(store, instance, "id-async")?;
            let second = func(store, instance, "deep-async")?;
            let (a_args, b_args) = ([a.clone()], [b.clone()]);
            let (mut a_got, mut b_got) = ([Val::Bool(false)], [Val::Bool(false)]);
            store
                .run_concurrent(async |accessor| {
                    let (x, y) = join(
                        first.call_concurrent(accessor, &a_args, &mut a_got),
                        second.call_concurrent(accessor, &b_args, &mut b_got),
                    )
                    .await;
                    x?;
                    y
                })
                .await??;
            anyhow::Ok((a_got[0].clone(), b_got[0].clone()))
        }
        .await;
        match outcome {
            Err(e) => self.fail("concurrent", ty, format!("{e:#}")),
            Ok((a_got, b_got)) => {
                if a_got != a || b_got != b {
                    self.fail(
                        "concurrent",
                        ty,
                        format!(
                            "mismatch\n    sent: {} / {}\n    got:  {} / {}",
                            show(&a),
                            show(&b),
                            show(&a_got),
                            show(&b_got)
                        ),
                    );
                }
            }
        }
    }

    /// A fill argument: random bytes, or all ones, of `size`.
    fn fill(&mut self, size: usize) -> Val {
        let bytes = if self.rng.chance(50) {
            self.rng.bytes(size)
        } else {
            vec![0xFF; size]
        };
        Val::List(bytes.into_iter().map(Val::U8).collect())
    }

    /// Report every failure, by type and then crossing, with the first
    /// failure of each, and fail if there were any.
    pub fn finish(self) -> Result<()> {
        eprintln!(
            "{} types, {} calls, {} failures",
            self.types,
            self.calls,
            self.failures.len()
        );
        eprintln!("limits crossed, in types:");
        for (label, count) in &self.crossed {
            eprintln!("  {count:>4}  {label}");
        }
        if self.failures.is_empty() {
            return Ok(());
        }
        // Grouped by type, then crossing. A stable sort keeps each group's
        // failures in the order they happened, so the first is shown.
        let mut failures: Vec<&Failure> = self.failures.iter().collect();
        failures.sort_by(|a, b| (&a.ty, &a.crossing).cmp(&(&b.ty, &b.crossing)));
        for of_type in failures.chunk_by(|a, b| a.ty == b.ty) {
            eprintln!("\n== {}", of_type[0].ty);
            for of_crossing in of_type.chunk_by(|a, b| a.crossing == b.crossing) {
                eprintln!(
                    "  {} x{}: {}",
                    of_crossing[0].crossing,
                    of_crossing.len(),
                    of_crossing[0].detail.replace('\n', "\n      ")
                );
            }
        }
        bail!("{} failure(s)", self.failures.len())
    }
}

/// A store with its hostcall fuel set here, not left at wasmtime's default.
/// A host receiving a value through the `Val` API pays about
/// `size_of::<Val>()` per value: each list element, record field and tuple
/// member, plus a field's name, or a string's bytes (at most 4 per
/// character). A generated value holds at most [`MAX_ELEMENTS`] such values
/// in its variable-length parts, plus its type's fixed ones (which `FIXED`
/// covers), and crosses at most twice per call (`twice`). The factor of 2
/// beyond that covers names.
fn new_store(engine: &Engine) -> Store<()> {
    /// The fixed values of the widest generated types, with room to spare.
    const FIXED: usize = 10_000;
    let mut store = Store::new(engine, ());
    store.set_hostcall_fuel(2 * 2 * (MAX_ELEMENTS + FIXED) * std::mem::size_of::<Val>());
    store
}

fn linker(engine: &Engine, host: &Arc<Mutex<Host>>) -> Result<Linker<()>> {
    let mut linker = Linker::<()>::new(engine);
    let mut src = linker.instance("test:abi/src")?;
    let get_host = host.clone();
    src.func_new("get", move |_, _, _, results| {
        results[0] = get_host
            .lock()
            .unwrap()
            .next
            .clone()
            .ok_or_else(|| wasmtime::Error::msg("no value to get"))?;
        Ok(())
    })?;
    let get_async_host = host.clone();
    src.func_new_concurrent("get-async", move |_, _, _, results| {
        let host = get_async_host.clone();
        Box::pin(async move {
            if host.lock().unwrap().block {
                BlockOnce { blocked: false }.await;
            }
            results[0] = host
                .lock()
                .unwrap()
                .next
                .clone()
                .ok_or_else(|| wasmtime::Error::msg("no value to get"))?;
            Ok(())
        })
    })?;
    let put_host = host.clone();
    linker
        .instance("test:abi/sink")?
        .func_new("put", move |_, _, args, _| {
            put_host.lock().unwrap().received = Some(args[0].clone());
            Ok(())
        })?;
    Ok(linker)
}

fn func(store: &mut Store<()>, instance: &Instance, name: &str) -> Result<Func> {
    instance
        .get_func(&mut *store, name)
        .with_context(|| format!("the {name} export"))
}

async fn call(
    store: &mut Store<()>,
    instance: &Instance,
    name: &str,
    args: Vec<Val>,
    results: usize,
) -> Result<Vec<Val>> {
    let function = func(store, instance, name)?;
    let mut got = vec![Val::Bool(false); results];
    store
        .run_concurrent(async |accessor| function.call_concurrent(accessor, &args, &mut got).await)
        .await??;
    Ok(got)
}

/// A type as WIT: its expression, then the definitions it uses.
fn describe(ty: &Ty) -> String {
    let mut definitions = Vec::new();
    ty.definitions(&mut definitions);
    if definitions.is_empty() {
        ty.wit()
    } else {
        format!("{}   where {}", ty.wit(), definitions.join("; "))
    }
}

/// A value for a report, cut short if necessary.
fn show(v: &Val) -> String {
    let text = format!("{v:?}");
    if text.len() > 300 {
        format!(
            "{}... ({} chars)",
            &text[..text.floor_char_boundary(300)],
            text.len()
        )
    } else {
        text
    }
}

/// Not ready the first time it is polled, so a call awaiting it blocks.
struct BlockOnce {
    blocked: bool,
}

impl Future for BlockOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        if self.blocked {
            return Poll::Ready(());
        }
        self.blocked = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}
