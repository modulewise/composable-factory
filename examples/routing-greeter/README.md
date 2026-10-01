# Routing Greeter Factory Example

A factory that generates a greeter which routes each call, by locale, to the greeter for that
locale. Its config maps each locale to a greeter, matched exactly or as a prefix. A locale no route
matches goes to `english`.

## Prerequisites

**Rust with the `wasm32-unknown-unknown` target**, to build the factory and the greeter:

```bash
rustup target add wasm32-unknown-unknown
```

**[`wasm-tools`](https://github.com/bytecodealliance/wasm-tools)**, to turn their core modules into
components:

```bash
cargo install wasm-tools
```

**[`composable`](https://github.com/modulewise/composable-runtime)**, to run the factory and to
call the component it produces:

```bash
cargo install --git https://github.com/modulewise/composable-runtime --branch main --locked composable-runtime
```

## The WIT

Each greeter implements one interface, from [`wit/package.wit`](wit/package.wit):

```wit
interface greeter {
    greet: func(name: string) -> string;
}
```

The generated routing greeter exports a function that also takes the locale:

```wit
world routing-greeter {
    export greet: func(name: string, locale: string) -> string;
}
```

## The Config

```toml
[component.routing-greeter-factory.config]
"en-AU" = "australian"
"es-*" = "spanish"
"fr" = "french"
```

Each key is a locale and each value is the greeter it routes to. A locale ending in `*` is a
prefix, so `es-*` matches `es-MX` and `es-ES`. The others match exactly.

## The World

```rust
fn build_world(&self, world: &mut World) -> Result<()> {
    let greeter = PackageSource::from_text(GREETER_WIT)?;
    for name in self.greeters() {
        world.add_imports(greeter.interface("greeter")?.named(name)?)?;
    }
    let routing_greeter = PackageSource::from_text(ROUTING_GREETER_WIT)?;
    world.add_exports(routing_greeter.world("routing-greeter")?.exports())
}
```

`named` gives each import of the `greeter` interface its own name, so the generated component
imports one greeter per name the config uses, plus `english`:

```wit
import australian: example:routing-greeter/greeter;
import english: example:routing-greeter/greeter;
import french: example:routing-greeter/greeter;
import spanish: example:routing-greeter/greeter;
export greet: func(name: string, locale: string) -> string;
```

## The Function

```rust
let arms = self
    .routes
    .iter()
    .map(|route| match route.is_prefix {
        true => prefix(&route.locale, move |_| greet(&route.greeter)),
        false => exact(&route.locale, move || greet(&route.greeter)),
    })
    .collect();
locale.match_string(arms, || greet(FALLBACK))
```

`match_string` tries the arms in order and runs the first that matches, or `otherwise` (here, the
`english` greeter) if none does. Each arm calls its greeter and returns the greeting.

Config entries have no reliable order, so the factory sorts the routes: exact first, then prefixes
from longest to shortest. That puts a specific route like `es-MX` before a broader one like `es-*`,
which `match_string` requires.

## Running the Example

```bash
./run.sh
```

It builds the factory and the greeter, then greets in four locales:

```
==> Invoking the routing greeter:
    greet world en-AU  => "g'day world!"
    greet world es-MX  => "hola world!"
    greet world fr     => "bonjour world!"
    greet world de-DE  => "hello world!"
```

`en-AU` and `fr` match exactly, `es-MX` matches the `es-*` prefix, and `de-DE` matches no route,
so it goes to `english`.

Each greeter component has a `greeting` config value, or `hello` as a default. Four such components
are defined in `config.toml`: `english`, `australian`, `spanish` and `french`. Each of the routing
greeter's imports is satisfied by the component whose name matches the named import.
