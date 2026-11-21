# GitHub API Example

Uses the [url-api-factory](../../components/url-api-factory) to generate a
component with three typed functions over public
[GitHub REST API](https://docs.github.com/en/rest) endpoints for repositories,
reading their JSON responses into WIT types.

## Prerequisites

**[`composable`](https://github.com/modulewise/composable-runtime)**, to run
the factory and to call the component it produces:

```bash
cargo install --git https://github.com/modulewise/composable-runtime --branch main --locked composable-runtime
```

**[`jq`](https://jqlang.org)**, which `run.sh` uses to show a few fields of
each response.

## The Config

[`config.toml`](config.toml) declares the three endpoints, for example:

```toml
[component.github-api-factory.config.endpoints.get-repository]
template = "GET https://api.github.com/repos/{owner}/{repo}"
result = "repository"
```

Its `types` are projections of GitHub's JSON with only the fields the functions
return. GitHub's JSON names are snake_case:

```toml
[component.github-api-factory.config.names]
fields = "snake_case"   # stargazers-count <- stargazers_count
```

GitHub requires a `User-Agent` header, and asks for its media type in `Accept`
and the API version in `X-GitHub-Api-Version`, all sent with every request.

The requests have no credentials, so they reach only public repositories.
GitHub allows 60 such requests an hour for each IP address, and searches 10 a
minute.

The generated component exports `example:github/github` where each endpoint
is mapped to a function:

```wit
get-repository: async func(owner: string, repo: string) -> result<repository, http-error>;
search-repositories: async func(query: string, limit: u32) -> result<search-result, http-error>;
get-latest-release: async func(owner: string, repo: string) -> result<release, http-error>;
```

`search-repositories` returns the repositories that match `query`, most stars
first. The query uses GitHub's search syntax, with qualifiers separated by
spaces, such as `topic:webassembly language:rust`, or `user:bytecodealliance`
for one owner's repositories.

## Running the Example

```bash
./run.sh
```

It calls each function: `get-repository` and `get-latest-release` for
bytecodealliance/wasmtime, and `search-repositories` for the three Rust
repositories with the most stars in the `webassembly` topic:

```
==> Invoking the github component:
    get-repository bytecodealliance wasmtime
{
  "full-name": "bytecodealliance/wasmtime",
  "stargazers-count": 18885,
  ...
    get-latest-release bytecodealliance wasmtime
{
  "tag-name": "v51.0.0",
  ...
    search-repositories "topic:webassembly language:rust" 3
{
  "total-count": 3146,
  "items": [
  ...
```

Others to try:

```bash
composable invoke config.toml -- github.github.get-repository modulewise composable-runtime
composable invoke config.toml -- github.github.search-repositories "user:bytecodealliance" 5
```
