wit_bindgen::generate!({
    path: "wit",
    world: "filesystem-loader",
    generate_all,
});

use wasi::filesystem::types::{DescriptorFlags, OpenFlags, PathFlags};

struct FilesystemLoader;

impl exports::composable::factory::loader::Guest for FilesystemLoader {
    async fn load(source: String) -> Result<Vec<u8>, String> {
        // Match the base path to find the right preopened directory.
        let (directory, path) = wasi::filesystem::preopens::get_directories()
            .into_iter()
            .find_map(|(directory, base_path)| {
                let path = source.strip_prefix(&base_path)?.strip_prefix('/')?;
                Some((directory, path.to_string()))
            })
            .ok_or_else(|| format!("filesystem-loader: no preopen contains '{source}'"))?;
        let file = directory
            .open_at(
                PathFlags::empty(),
                path,
                OpenFlags::empty(),
                DescriptorFlags::READ,
            )
            .await
            .map_err(|e| format!("filesystem-loader: cannot open '{source}': {e}"))?;

        let (stream, result) = file.read_via_stream(0);
        let contents = stream.collect().await;
        result
            .await
            .map_err(|e| format!("filesystem-loader: cannot read '{source}': {e}"))?;

        Ok(contents)
    }
}

export!(FilesystemLoader);
