# Weather API Example

Uses the [url-api-factory](../../components/url-api-factory) to generate a
component with five typed functions over
[api.weather.gov](https://www.weather.gov/documentation/services-web-api)
endpoints, reading their JSON responses into WIT types.

## Prerequisites

**Rust with the `wasm32-unknown-unknown` target**, to build the components:

```bash
rustup target add wasm32-unknown-unknown
```

**[`wasm-tools`](https://github.com/bytecodealliance/wasm-tools)**, to turn
their core modules into components:

```bash
cargo install wasm-tools
```

**[`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools)**, to pull the
HTTP client component:

```bash
cargo install wkg
```

**[`composable`](https://github.com/modulewise/composable-runtime)**, to run
the factory and to call the component it produces:

```bash
cargo install --git https://github.com/modulewise/composable-runtime --branch main --locked composable-runtime
```

**[`jq`](https://jqlang.org)**, which `run.sh` uses to read the grid point from
one response and pass it to the next call.

## The Config

[`config.toml`](config.toml) declares the five endpoints, for example:

```toml
[component.weather-api-factory.config.endpoints.get-forecast]
template = "GET https://api.weather.gov/gridpoints/{office}/{grid-x:u32},{grid-y:u32}/forecast"
result = "forecast"
```

Its `types` are projections of weather.gov's GeoJSON: only the fields the
functions return, nested as the documents nest them. weather.gov's JSON names
are camelCase for fields and PascalCase for enum values:

```toml
[component.weather-api-factory.config.names]
fields = "camelCase"   # short-forecast <- shortForecast
enums = "PascalCase"   # all-clear <- AllClear
```

weather.gov requires a `User-Agent` header, which is sent with every request.

The generated component exports `example:weather/weather` where each endpoint
is mapped to a function, for example:

```wit
get-forecast: async func(office: string, grid-x: u32, grid-y: u32) -> result<forecast, http-error>;
```

## Running the Example

```bash
./run.sh
```

It builds the components, then calls three of the functions: `get-point` for
Boston, `get-forecast` with the grid point from that response (showing its
first two periods), and `get-alerts` for Massachusetts:

```
==> Invoking the weather component:
    get-point 42.3611 -71.0571
{
  "properties": {
    "grid-id": "BOX",
    "grid-x": 71,
    "grid-y": 101,
    ...
    get-forecast BOX 71 101
[
  {
    "number": 1,
    "name": "Today",
    ...
```

The others:

```bash
composable invoke config.toml -- weather.weather.get-hourly-forecast BOX 71 101
composable invoke config.toml -- weather.weather.get-latest-observation KBOS
```
