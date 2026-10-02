//! A factory whose heap is reset once no task is in progress, and only then.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasm_encoder::Instruction;
use wasmtime::component::{Component, Func, Instance, Linker, Val};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

const TASKS_WIT: &str = r"package test:tasks;
    interface gate { wait: async func(); }
    world tasks {
      import gate;
      export echo: func(data: list<u8>) -> list<u8>;
      export take: func(data: list<u8>);
      export take-early: func(data: list<u8>);
      export echo-later: async func(data: list<u8>) -> list<u8>;
    }";

/// The size of each call's argument.
const SIZE: usize = 100_000;

/// Enough memory for the calls in progress at once, not for 20 without resets.
const MEMORY_LIMIT: usize = 512 * 1024;

/// The byte `echo-later` returns ahead of its argument.
const MARKER: u8 = 0xFF;

/// `echo` returns its argument. `take` returns nothing, and `take-early` does
/// too, from a `return` emitted through its body. `echo-later` waits at
/// the `gate`, then returns [`MARKER`] followed by its argument twice. That
/// result is allocated after the wait, and the marker is written first, so a
/// heap reset during the wait would have the marker overwrite the start of the
/// argument.
struct Tasks;

/// What `echo-later` returns for `data`.
fn marked(data: &[Val]) -> Vec<Val> {
    std::iter::once(Val::U8(MARKER))
        .chain(data.iter().cloned())
        .chain(data.iter().cloned())
        .collect()
}

impl ComponentBuilder for Tasks {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let tasks = PackageSource::from_text(TASKS_WIT)?.world("tasks")?;
        world.add_imports(tasks.imports())?;
        world.add_exports(tasks.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let data = function.param("data")?.receive()?;
        let result = || function.result().context("the function returns a list");
        match function.name() {
            "echo" => result()?.value().write(&ValueSpec::from(&data)),
            "take" => Ok(()),
            "take-early" => {
                function.body().emit(Instruction::Return);
                Ok(())
            }
            _ => {
                imports.interface("gate")?.function("wait")?.call(&[])?;
                result()?.value().write(&ValueSpec::concat([
                    ValueSpec::list([ValueSpec::u8(MARKER)]),
                    ValueSpec::from(&data),
                    ValueSpec::from(&data),
                ]))
            }
        }
    }
}

/// A store's data: the limits its memory is held to.
struct Limited {
    limits: StoreLimits,
}

/// An instance whose `gate` opens once `open` is set, in a store held to
/// [`MEMORY_LIMIT`].
async fn instantiate(open: Arc<AtomicBool>) -> Result<(Store<Limited>, Instance)> {
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, build(&Tasks)?)?;
    let mut linker = Linker::<Limited>::new(&engine);
    linker
        .instance("test:tasks/gate")?
        .func_new_concurrent("wait", move |_, _, _, _| {
            let open = open.clone();
            Box::pin(async move {
                Gate { open }.await;
                Ok(())
            })
        })?;
    let limits = StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).build();
    let mut store = Store::new(&engine, Limited { limits });
    store.limiter(|data| &mut data.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}

fn function(store: &mut Store<Limited>, instance: &Instance, name: &str) -> Result<Func> {
    instance
        .get_func(&mut *store, name)
        .with_context(|| format!("the {name} function"))
}

/// A list of [`SIZE`] bytes, each `seed` plus its index.
fn data(seed: u8) -> Vec<Val> {
    (0..SIZE)
        .map(|i| Val::U8(seed.wrapping_add(i as u8)))
        .collect()
}

/// Call `name` 20 times, each with a new argument, checking each result.
fn call_repeatedly(name: &str, expected: impl Fn(Vec<Val>) -> Vec<Val>) -> Result<()> {
    block_on(async {
        let (mut store, instance) = instantiate(Arc::new(AtomicBool::new(true))).await?;
        let function = function(&mut store, &instance, name)?;
        for seed in 0..20 {
            let data = data(seed);
            let mut results = vec![Val::Bool(false); function.ty(&store).results().len()];
            store
                .run_concurrent(async |accessor| {
                    function
                        .call_concurrent(accessor, &[Val::List(data.clone())], &mut results)
                        .await
                })
                .await??;
            if let Some(result) = results.first() {
                assert_eq!(result, &Val::List(expected(data)), "call {seed}");
            }
        }
        Ok(())
    })
}

#[test]
fn memory_is_reused_across_sync_calls() -> Result<()> {
    // Each export's post-return ends its task.
    // A result through a pointer.
    call_repeatedly("echo", |data| data)?;
    // No result.
    call_repeatedly("take", |data| data)?;
    // No result, from a body that returns early.
    call_repeatedly("take-early", |data| data)
}

#[test]
fn memory_is_reused_across_async_calls() -> Result<()> {
    call_repeatedly("echo-later", |data| marked(&data))
}

#[test]
fn an_async_call_keeps_its_memory_when_a_sync_call_ends() -> Result<()> {
    block_on(async {
        let open = Arc::new(AtomicBool::new(false));
        let (mut store, instance) = instantiate(open.clone()).await?;
        let echo_later = function(&mut store, &instance, "echo-later")?;
        let echo = function(&mut store, &instance, "echo")?;
        let (first, second) = (data(1), data(2));
        let first_args = [Val::List(first.clone())];
        let second_args = [Val::List(second.clone())];
        let mut later = [Val::Bool(false)];
        let mut now = [Val::Bool(false)];
        store
            .run_concurrent(async |accessor| {
                // `echo-later` waits at the gate, which opens only once `echo`
                // has returned, and its post-return has run.
                let waiting = echo_later.call_concurrent(accessor, &first_args, &mut later);
                let meanwhile = async {
                    echo.call_concurrent(accessor, &second_args, &mut now)
                        .await?;
                    open.store(true, Ordering::SeqCst);
                    anyhow::Ok(())
                };
                let (waited, ran) = join(waiting, meanwhile).await;
                waited?;
                ran
            })
            .await??;
        assert_eq!(now[0], Val::List(second));
        assert_eq!(later[0], Val::List(marked(&first)));
        Ok(())
    })
}

/// A future that is ready once `open` is set, polling until it is.
struct Gate {
    open: Arc<AtomicBool>,
}

impl Future for Gate {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        if self.open.load(Ordering::SeqCst) {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// Run `first` and `second` together until both are complete.
async fn join<A, B>(first: impl Future<Output = A>, second: impl Future<Output = B>) -> (A, B) {
    let mut first = std::pin::pin!(first);
    let mut second = std::pin::pin!(second);
    let (mut a, mut b) = (None, None);
    std::future::poll_fn(|cx| {
        if a.is_none()
            && let Poll::Ready(output) = first.as_mut().poll(cx)
        {
            a = Some(output);
        }
        if b.is_none()
            && let Poll::Ready(output) = second.as_mut().poll(cx)
        {
            b = Some(output);
        }
        if a.is_some() && b.is_some() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    (
        a.expect("first is complete"),
        b.expect("second is complete"),
    )
}

/// Run `future` to completion on this thread, parking it while the future is
/// not ready.
fn block_on<T>(future: impl Future<Output = T>) -> T {
    struct Unpark(std::thread::Thread);

    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Arc::new(Unpark(std::thread::current())).into();
    let mut cx = TaskContext::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::park();
    }
}
