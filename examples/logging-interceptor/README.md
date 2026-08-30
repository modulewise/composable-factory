# Logging Interceptor Factory

A factory that creates an interceptor that mirrors an arbitrary target component's exports,
forwards to its imports, and logs each function call and return.

## Prerequisites

**Rust with the `wasm32-unknown-unknown` target**, to build the factory and the loader:

```bash
rustup target add wasm32-unknown-unknown
```

**[`wasm-tools`](https://github.com/bytecodealliance/wasm-tools)**, to turn core modules into
components and to inspect what the factory emits:

```bash
cargo install wasm-tools
```

**[`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools)**, to pull the target component and a
`wasi:logging` implementation from their registries:

```bash
cargo install wkg
```

**[`composable`](https://github.com/modulewise/composable-runtime)**, to run the factory and to
call the component it produces:

```bash
cargo install --git https://github.com/modulewise/composable-runtime --branch main --locked composable-runtime
```

## Providing the Target

The `Builder` struct holds the bytes of a target component:

```rust
pub struct Builder {
    target: Vec<u8>,
}
```

Those bytes are fetched by a loader, whose import is defined in the world:

```wit
world logging-interceptor-factory {
    import wasi:config/store@0.2.0-rc.1;   // provides path to target
    import composable:factory/loader;      // loads the target bytes
    export composable:factory/factory;
}
```

The exported `build` reads the target's path from config, loads its bytes, and hands them to the
`Builder`:

```rust
async fn build() -> Result<Vec<u8>, String> {
    let source = wasi::config::store::get("target")
        .map_err(|e| format!("reading config 'target': {e:?}"))?
        .ok_or_else(|| "no target in config".to_string())?;

    let target = composable::factory::loader::load(source).await?;

    composable_factory::build(&Builder::new(target)).map_err(|e| format!("{e:#}"))
}
```

The target is specified in config, so intercepting a different component means
changing `factory-config.toml` rather than rebuilding the factory.

## Mirroring the Exports

```rust
fn build_world(&self, world: &mut World) -> Result<()> {
    let target = WorldSource::from_component(&self.target)?;
    if target.exports().is_empty() {
        anyhow::bail!("target has no exports to intercept");
    }

    world.add_imports(target.exports())?;   // call the target via imports
    world.add_exports(target.exports())?;   // surface the same exports

    let logging = PackageSource::from_text(LOGGING_WIT)?;
    world.add_imports(logging.interface(LOGGING_INTERFACE)?)    // make other calls
}
```

The same `target.exports()` selection is added to this component's world as both imports and
exports.

`WorldSource::from_component` decodes a component's bytes into the world it implements, failing if
the bytes are a WIT package rather than a component. `PackageSource::from_text` parses WIT source.

`wasi:logging` is an additional import used within each intercepted call.

## Building the Functions

```rust
fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
    log(imports, &name, "called")?;

    let args = function.params().iter()
        .map(|p| p.receive())
        .collect::<Result<Vec<_>>>()?;

    let result = target_function(imports, /* ... */)?.call(&args)?;

    log(imports, &name, "returned")?;

    match (function.result(), result) {
        (Some(dest), Some(value)) => dest.value().write(&ValueSpec::from(value)),
        _ => Ok(()),
    }
}
```

The following API calls support this build step:

- **`param.receive()`**: an inbound argument as a `Value`.
- **`imported.call(args)`**: invokes a function, returning a `Value`.
- **`value.write(spec)`**: define a `Value` that can be used in calls.

A `Value` is a common abstraction over different representations. Some values are in locals, others
are in linear memory, but the same `write` call handles either.

Building arguments:

```rust
let level = log_fn.param("level")?.value()?;
level.write(&ValueSpec::variant_unit(LEVEL))?;

let context_arg = log_fn.param("context")?.value()?;
context_arg.write(&ValueSpec::string(context))?;

let message_arg = log_fn.param("message")?.value()?;
message_arg.write(&ValueSpec::string(message))?;

log_fn.call(&[level, context_arg, message_arg])?;
```

`param.value()` reserves a slot, `write` fills it, `call` passes it. `variant_unit("info")`
specifies a variant case. `ValueSpec` provides constructors to accommodate different WIT types.

## Running the Example

```bash
./run.sh
```

It builds the factory, builds a filesystem loader to satisfy the `loader` import, pulls the
`hello` target component and a `wasi:logging` implementation, then generates and invokes:

```
==> Invoking the intercepted greeter:
2026-08-20 13:53:10.990Z INFO  [greet]: called
2026-08-20 13:53:10.990Z INFO  [greet]: returned
"Hello World!"
```

`factory-config.toml` wires the factory's imports and provides the target in config.

`greeter-config.toml` wires the generated component with the `hello` target component
and a logging implementation. The loader is restricted to a read-only preopen.
