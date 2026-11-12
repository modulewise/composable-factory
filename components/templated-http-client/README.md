# Templated HTTP Client

Expands a URI template with typed values, sends the request, and reads the
response from a stream. It is built on componentized's
[HTTP client](https://github.com/componentized/http).

## The Interface

Exports `composable:templated-http-client/client`:

```wit
request: async func(
    method: method,
    template: string,
    values: map<string, template-value>,
    headers: map<string, string>,
) -> result<response, error>;
```

- **`method`** is `get`, `head`, `delete` or `options`. Methods with a request
  body are not yet supported.
- **`template`** is expanded with `values` by
  [RFC 6570](https://www.rfc-editor.org/rfc/rfc6570) simple expansion. Each
  `{name}` is replaced by its value, percent-encoded except for unreserved
  characters. A variable with no value is an error. Operators such as
  `{?query}` are not yet supported.
- **`template-value`** is `text`, `signed` (s64), `unsigned` (u64), `float`
  (f64) or `boolean`.
- **`response`** includes status, headers, and the body as a string.
- **`error`** is either `response` for a status that is not 2xx or `failed`
  when there was no usable response: a bad template, a connection failure, a
  redirect limit, or a body that is not UTF-8.

See [`wit/client.wit`](wit/client.wit) for the full definitions.

## Configuring with Composable Runtime

```toml
[component.templated-http-client]
uri = "oci://ghcr.io/modulewise/component/templated-http-client:0.1.0"
imports = ["http-client"]

[component.http-client]
uri = "oci://ghcr.io/componentized/http/client:0.1.0"
imports = ["http", "clocks"]

[capability.http]
type = "wasi:http"

[capability.clocks]
type = "wasi:clocks"
```

## WIT Dependencies

`wit/deps` is fetched from the componentized.dev registry, which requires
the registry config as defined in
[componentized/http](https://github.com/componentized/http):

```bash
WKG_CONFIG_FILE=path/to/componentized-http/.config/wasm-pkg/config.toml wkg wit fetch
```

## Building

Run the following from the parent [components](../) directory:

```bash
./build.sh
```

That produces `templated-http-client.wasm` in that directory's `lib` directory.
