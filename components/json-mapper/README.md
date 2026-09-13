# JSON Mapper

Converts between JSON text and WIT values, so a factory-generated component can emit calls to this
rather than handling text at the wasm level itself.

## The Interfaces

Implements [`composable:factory/serializer`](../../wit/serializer.wit) and
[`composable:factory/deserializer`](../../wit/deserializer.wit).

**Serializer** (WIT to JSON) receives a push sequence and accumulates text:

```wit
begin-object: func();
add-key: func(name: string);
add-string: func(s: string);
end-object: func();
finish: func() -> string;
```

**Deserializer** (JSON to WIT) is a cursor the caller navigates:

```wit
constructor(input: string);
field-presence: func(names: list<string>) -> u32;
enter-field: func(name: string);
get-string: func() -> string;
exit: func();
```

## Representation

| WIT | JSON |
|---|---|
| `record` | object keyed by field name |
| `tuple` | array, one element per member |
| `list` | array |
| `map<string, V>` | object, since JSON keys are strings |
| `map<K, V>` | array of `[key, value]` arrays |
| `enum` | the case name as a string |
| `variant`, `option`, `result` | `{"type": name}`, plus `"value"` when the case has a payload |
| `flags` | array of the set names |

## Configuring with Composable Runtime

A generated component that maps JSON can refer to this component:

```toml
[component.tool]
uri = "factory:my-factory"
imports = ["mapper"]

[component.mapper]
uri = "oci://ghcr.io/modulewise/component/json-mapper:0.4.0"
```

The factory drives it through a `ReadVisitor` (WIT to JSON) or `WriteVisitor` (JSON to WIT).

## Building

Run the following from the parent [components](../) directory:

```bash
./build.sh
```

Produces `json-mapper.wasm` in that directory's `lib` sub-directory.
