# Filesystem Loader

Reads a component's bytes from a preopened directory, so a factory can load it.

## The Interface

Implements [`composable:factory/loader`](../../wit/loader.wit):

```wit
interface loader {
    load: async func(source: string) -> result<list<u8>, string>;
}
```

`source` is a guest path, resolved against the loader's preopens.

## Configuring with Composable Runtime

A factory that imports `composable:factory/loader` can refer to this component, and the host
capability can provide the preopens:

```toml
[component.factory]
uri = "./lib/my-factory.wasm"
imports = ["loader"]
config.target = "/lib/target.wasm"

[component.loader]
uri = "oci://ghcr.io/modulewise/component/filesystem-loader:0.3.0"
imports = ["filesystem"]

[capability.filesystem]
type = "wasi:filesystem"

[[capability.filesystem.preopens]]
host = "./lib"
guest = "/lib"
perms = "read-only"
```

See [logging-interceptor](../../examples/logging-interceptor) for a working example.

## Building

Run the following from the parent [components](../) directory:

```bash
./build.sh
```

Produces `filesystem-loader.wasm` in that directory's `lib` sub-directory.