use anyhow::Result;

use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports, ValueSpec};
use composable_factory::{ComponentBuilder, World};

const WIT: &str = r"package example:helloworld;

world helloworld {
  export say-hello: func(name: string) -> string;
}";

pub struct Builder;

impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let helloworld = PackageSource::from_text(WIT)?.world("helloworld")?;
        world.add_exports(helloworld.exports())
    }

    fn build_function(&self, function: &ExportedFunction, _imports: &Imports) -> Result<()> {
        let name = function.param("name")?.receive()?;
        function
            .result()
            .expect("say-hello returns a string")
            .value()
            .write(&ValueSpec::concat([
                ValueSpec::string("hello "),
                ValueSpec::from(name),
            ]))
    }
}

wit_bindgen::generate!({
    path: "../wit",
    world: "simple-factory",
});

struct Factory;

impl exports::composable::factory::factory::Guest for Factory {
    async fn build() -> Result<Vec<u8>, String> {
        composable_factory::build(&Builder).map_err(|e| format!("{e:#}"))
    }
}

export!(Factory);
