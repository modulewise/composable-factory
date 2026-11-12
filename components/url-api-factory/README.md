# URL API Factory

Generates a component with one typed function per configured HTTP endpoint.
Each endpoint is a function name, a request line with a URI template, and
optionally a result type that is mapped from the JSON response.

## Example Config

```toml
[component.weather-api-factory.config]
package = "example:weather"
interface = "weather"
types = '''
    record forecast { properties: forecast-properties }
    record forecast-properties { periods: list<period> }
    record period { name: string, temperature: s32, short-forecast: string }
'''

[component.weather-api-factory.config.names]
fields = "camelCase"

[component.weather-api-factory.config.headers]
User-Agent = "my-weather-app/0.1"

[component.weather-api-factory.config.endpoints.get-forecast]
template = "GET https://api.weather.gov/gridpoints/{office}/{grid-x:u32},{grid-y:u32}/forecast"
result = "forecast"
```

| Key | Meaning |
|---|---|
| `package` | The generated interface's package (default `generated:api`) |
| `interface` | The generated interface's name (default `endpoints`) |
| `types` | WIT type definitions for the result types |
| `names.fields` | The convention mapping WIT field names to JSON names |
| `names.enums` | The convention mapping WIT enum/variant cases and flags to JSON values |
| `overrides.fields.<name>` | The JSON name for one WIT field name |
| `overrides.enums.<name>` | The JSON value for one WIT enum case, variant case name, or flag |
| `headers.<name>` | A header sent with every request |
| `endpoints.<function>.template` | `METHOD uri-template` |
| `endpoints.<function>.result` | The result type (default `string`, the body itself) |

The naming conventions are `identity` (the default), `camelCase`, `PascalCase`
and `snake_case`. Words that are all uppercase in WIT keep their case.

### Templates

The method is `GET`, `HEAD`, `DELETE` or `OPTIONS` (methods with a request body
are not yet supported). Each template variable becomes a param, a `string`
unless it declares a type (`bool`, `s8`-`s64`, `u8`-`u64`, `f32` or `f64`):

```
GET https://api.weather.gov/points/{latitude:f64},{longitude:f64}
```

Variables use [RFC 6570](https://www.rfc-editor.org/rfc/rfc6570) simple
expansion. Operators such as `{?query}` are not yet supported.

### Result Types

Each type is a projection, only the fields to return, nested as in the JSON.
Fields in JSON that are not delcared in the type are ignored, and a field that
may be `null` or absent is an `option`.

## The Generated Component

```wit
get-forecast: async func(office: string, grid-x: u32, grid-y: u32) -> result<forecast, http-error>;
```

Every function returns `result<T, http-error>`:

```wit
record error-response { status: u16, body: string }
variant http-error { response(error-response), failed(string) }
```

A 2xx body is read into `T`. A response with any other status is `response`,
with its status and unparsed body, and `failed` reports why the response itself
was not usable.

It imports [templated-http-client](../templated-http-client) to send each
request and, when any result type is not `string`, it relies on its other
import, the `deserializer` from [json-mapper](../json-mapper).

```wit
import composable:templated-http-client/client@0.1.0;
import composable:factory/deserializer@0.4.0;
export example:weather/weather;
```

## Configuring with Composable Runtime

```toml
[component.weather]
uri = "factory:weather-api-factory"
imports = ["templated-http-client", "json-mapper"]

[component.weather-api-factory]
uri = "oci://ghcr.io/modulewise/component/url-api-factory:0.1.0"

[component.url-api-factory.config]
# as shown above

[component.templated-http-client]
uri = "oci://ghcr.io/modulewise/component/templated-http-client:0.1.0"
imports = ["http-client"]

[component.http-client]
uri = "oci://ghcr.io/componentized/http/client:0.1.0"
imports = ["http", "clocks"]

[component.json-mapper]
uri = "oci://ghcr.io/modulewise/component/json-mapper:0.4.0"

[capability.http]
type = "wasi:http"

[capability.clocks]
type = "wasi:clocks"
```

See the [weather-api example](../../examples/weather-api) for complete config.

## Building

Run the following from the parent [components](../) directory:

```bash
./build.sh
```

That produces `url-api-factory.wasm` in that directory's `lib` directory.
