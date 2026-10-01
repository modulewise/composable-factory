//! A factory whose async export calls an async import, which blocks before
//! returning its result.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World, build};
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Config, Engine, Store};

const RELAY_WIT: &str = r"package test:relay;
    interface source { get: async func() -> string; }
    world relay {
      import source;
      export relay: async func() -> string;
    }";

/// `relay` returns what `source.get` returns.
struct Relay;

impl ComponentBuilder for Relay {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let relay = PackageSource::from_text(RELAY_WIT)?.world("relay")?;
        world.add_imports(relay.imports())?;
        world.add_exports(relay.exports())
    }

    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
        let got = imports
            .interface("source")?
            .function("get")?
            .call(&[])?
            .context("get returns a string")?;
        function
            .result()
            .context("relay returns a string")?
            .value()
            .write(&ValueSpec::from(got))
    }
}

/// A future that is not ready the first time it is polled, and records that
/// it was not, so the call it completes is known to have blocked.
struct BlockOnce {
    blocked: Arc<AtomicBool>,
}

impl Future for BlockOnce {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<()> {
        if self.blocked.swap(true, Ordering::SeqCst) {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[test]
fn an_async_export_blocks_on_an_async_import() -> Result<()> {
    let bytes = build(&Relay)?;

    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, &bytes)?;

    let blocked = Arc::new(AtomicBool::new(false));
    let mut linker = Linker::<()>::new(&engine);
    let host_blocked = blocked.clone();
    linker
        .instance("test:relay/source")?
        .func_new_concurrent("get", move |_, _, _, results| {
            let blocked = host_blocked.clone();
            Box::pin(async move {
                BlockOnce { blocked }.await;
                results[0] = Val::String("from the host".to_string());
                Ok(())
            })
        })?;

    // A stackful async export needs an async store, driven by an executor.
    let results = block_on(async {
        let mut store = Store::new(&engine, ());
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let relay = instance
            .get_func(&mut store, "relay")
            .context("the relay function")?;
        let mut results = [Val::Bool(false)];
        relay.call_async(&mut store, &[], &mut results).await?;
        anyhow::Ok(results)
    })?;
    assert!(
        blocked.load(Ordering::SeqCst),
        "get blocked before returning"
    );
    assert_eq!(results[0], Val::String("from the host".to_string()));
    Ok(())
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
