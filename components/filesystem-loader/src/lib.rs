wit_bindgen::generate!({
    path: "wit",
    world: "filesystem-loader",
    generate_all,
});

use wasi::filesystem::types::{Descriptor, DescriptorFlags, OpenFlags, PathFlags};

struct FilesystemLoader;

fn preopen() -> Result<Descriptor, String> {
    wasi::filesystem::preopens::get_directories()
        .into_iter()
        .next()
        .map(|(descriptor, _path)| descriptor)
        .ok_or_else(|| "filesystem-loader: no preopened directory".to_string())
}

impl exports::composable::factory::loader::Guest for FilesystemLoader {
    async fn load(source: String) -> Result<Vec<u8>, String> {
        let file = preopen()?
            .open_at(
                PathFlags::empty(),
                source.clone(),
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
