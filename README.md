# Composable Factory

Build Wasm Components with Wasm Components

(Work-in-Progress)

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

## Examples

- [calculator](./examples/calculator): a factory that generates a 4-function calculator,
emitting Wasm instructions directly.
- [logging-interceptor](./examples/logging-interceptor): a factory that mirrors an arbitrary
target component's exports, forwards to them as imports, and logs each call and return.

## License

Copyright (c) 2026 Modulewise Inc and the Composable Factory contributors.

Apache License v2.0: see [LICENSE](./LICENSE) for details. 
