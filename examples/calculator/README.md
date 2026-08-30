# Calculator Factory Example

A simple factory that generates a 4-function calculator component from WIT and imports nothing.

## Prerequisites

**Rust with the `wasm32-unknown-unknown` target**, to build the factory itself:

```bash
rustup target add wasm32-unknown-unknown
```

**[`wasm-tools`](https://github.com/bytecodealliance/wasm-tools)**, to turn the factory's core
module into a component and to inspect what it emits:

```bash
cargo install wasm-tools
```

**[`composable`](https://github.com/modulewise/composable-runtime)**, to run the factory and to
call the component it produces:

```bash
cargo install --git https://github.com/modulewise/composable-runtime --branch main --locked composable-runtime
```

## The API

A factory is a component that emits another component. You implement `ComponentBuilder`:

```rust
impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()>;
    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()>;
}
```

`build_world` declares the generated component's surface.

`build_function` is then called once per exported function to emit its body.

## The World

```rust
fn build_world(&self, world: &mut World) -> Result<()> {
    let calc = PackageSource::from_wit(CALCULATOR_WIT)?.world("calculator")?;
    world.add_exports(calc.exports())
}
```

`World` has two methods: `add_imports` and `add_exports`. The WIT here is a constant string
embedded into the factory. It could have been fetched at build time (see
[logging-interceptor](../logging-interceptor) for an example that reads its target's WIT out of a
component it loads).

## The Functions

```rust
fn build_function(&self, function: &ExportedFunction, _imports: &Imports) -> Result<()> {
    let op = match function.name() {
        "add" => Instruction::I32Add,
        "subtract" => Instruction::I32Sub,
        "multiply" => Instruction::I32Mul,
        "divide" => Instruction::I32DivS,
        other => anyhow::bail!("unexpected calculator function '{other}'"),
    };

    let body = function.body();
    body.emit(Instruction::LocalGet(0));
    body.emit(Instruction::LocalGet(1));
    body.emit(op);
    Ok(())
}
```

`function` is the export being implemented, and `imports` is what the generated component may call.
The calculator doesn't call any functions, so it doesn't need any imports.

Both params are `s32` and passed in locals 0 and 1. A `LocalGet` pushes them onto the stack. The
operation pops them from the stack to use as its operands and then pushes its result onto the
stack. When each operation's function ends, that value left on the stack is the return value.

The `emit` function is the low-level "escape hatch" of the composable-factory API, but it is
sufficient here. Anything with a string, a variant, a list, or a record should use the navigable
world model instead (`param.receive()`, `value.write(spec)`). Those handle layout and ABI concerns,
as demonstrated in the [logging-interceptor](../logging-interceptor) example.

## Calling The Factory

```rust
async fn build() -> Result<Vec<u8>, String> {
    composable_factory::build(&Builder).map_err(|e| format!("{e:#}"))
}
```

The WIT contract is `build: async func() -> result<list<u8>, string>`. Everything a factory needs
should arrive through its imports, so it takes no arguments.

## Running the Example

```bash
./run.sh
```

It builds the factory, generates the calculator component, then invokes it:

```
==> Invoking the calculator:
    calc.add 2 3       => 5
    calc.multiply 6 7  => 42
```

`build.sh` compiles the factory itself: `cargo build` for a core module, then
`wasm-tools component new` to componentize it.

`run.sh` then uses `composable invoke` to call the
factory's `build` export, capturing the component it emits to stdout.
