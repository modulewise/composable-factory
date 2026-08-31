# Composable Factory

Build Wasm Components with Wasm Components

## The API

A Factory generates a Wasm Component. An implementor provides a `ComponentBuilder`:

```rust
impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()>;
    fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()>;
}
```

- `build_world` declares the generated component's surface.
- `build_function` is then called once per exported function to emit its body.

Passing a `ComponentBuilder` to `build` invokes both callbacks and returns the encoded component:

```rust
composable_factory::build(&Builder)  // -> Result<Vec<u8>>
```

## The Factory as a Component

A Factory can itself be built as a Wasm Component. The WIT interface to export is
[`composable:factory/factory`](./wit/factory.wit), and its `build` function returns the bytes of
the component it generates:

```wit
build: async func() -> result<list<u8>, string>
```

Since that function takes no arguments, a Factory Component relies on its own imports for
configuration, for fetching dynamic information needed to declare its target World, and for any
functionality it calls at build time.

## Examples

- [helloworld](./examples/helloworld): generates a greeter component, joining a literal greeting
with a received "name" value.
- [calculator](./examples/calculator): generates a 4-function calculator component, emitting Wasm
instructions directly.
- [logging-interceptor](./examples/logging-interceptor): generates a component that mirrors a
target component's exports, forwards to them as imports, and logs each call and return.

Each has a `run.sh` that builds the factory, generates a component with the factory, and then
invokes the generated component.

## License

Copyright (c) 2026 Modulewise Inc and the Composable Factory contributors.

Apache License v2.0: see [LICENSE](./LICENSE) for details. 
