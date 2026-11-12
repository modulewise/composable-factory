//! A factory that generates a component calling HTTP endpoints, one typed
//! function per configured URL template.

wit_bindgen::generate!({
    path: "wit",
    world: "url-api-factory",
    generate_all,
});

struct Factory;

impl exports::composable::factory::factory::Guest for Factory {
    async fn build() -> Result<Vec<u8>, String> {
        let entries =
            wasi::config::store::get_all().map_err(|e| format!("reading config: {e:?}"))?;
        http_api_adapter::build(entries).map_err(|e| format!("{e:#}"))
    }
}

export!(Factory);
