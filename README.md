# Composable Factory

Build Wasm Components with Wasm Components

(Work-in-Progress)

## Examples

- [calculator](./examples/calculator): a factory that generates a four-function calculator,
emitting Wasm instructions directly.
- [logging-interceptor](./examples/logging-interceptor): a factory that mirrors an arbitrary
target component's exports, forwards to them as imports, and logs each call and return.

## License

Copyright (c) 2026 Modulewise Inc and the Composable Factory contributors.

Apache License v2.0: see [LICENSE](./LICENSE) for details. 
