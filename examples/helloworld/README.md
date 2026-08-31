# Hello World Factory Example

A factory that generates a greeter component, joining a literal greeting with a received value.

## Prerequisites

**Rust with the `wasm32-unknown-unknown` target**, to build the factory:

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

## The World

```rust
const WIT: &str = r"package example:helloworld;

world helloworld {
  export say-hello: func(name: string) -> string;
}";

fn build_world(&self, world: &mut World) -> Result<()> {
    let helloworld = PackageSource::from_text(WIT)?.world("helloworld")?;
    world.add_exports(helloworld.exports())
}
```

The generated component exports one function and imports nothing.

## The Function

```rust
fn build_function(&self, function: &ExportedFunction, _imports: &Imports) -> Result<()> {
    let name = function.param("name")?.receive()?;
    function
        .result()
        .expect("say-hello returns a string")
        .value()
        .write(&ValueSpec::concat([
            ValueSpec::string("hello "),
            ValueSpec::from(name),
        ]))
}
```

`param.receive()` is a reference to the argument value the caller passed.

A `ValueSpec` enables writing a function's result value, and `ValueSpec::concat` joins parts into
one string. Unlike the literal, the param value's content, and thus length, is only known when the
generated component runs. The concatenated string is stored in a single allocation.

## Running the Example

```bash
./run.sh
```

It builds the factory, generates the greeter, then invokes it:

```
==> Invoking the greeter:
    greeter.say-hello world => "hello world"
```
