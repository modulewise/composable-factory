use anyhow::Result;
use wasm_encoder::Instruction;

use composable_factory::wit::PackageSource;
use composable_factory::world::{ExportedFunction, Imports};
use composable_factory::{ComponentBuilder, World};

const CALCULATOR_WIT: &str = r"package example:calculator;

world calculator {
  export add: func(a: s32, b: s32) -> s32;
  export subtract: func(a: s32, b: s32) -> s32;
  export multiply: func(a: s32, b: s32) -> s32;
  export divide: func(a: s32, b: s32) -> s32;
}";

pub struct Builder;

impl ComponentBuilder for Builder {
    fn build_world(&self, world: &mut World) -> Result<()> {
        let calc = PackageSource::from_text(CALCULATOR_WIT)?.world("calculator")?;
        world.add_exports(calc.exports())
    }

    fn build_function(&self, function: &ExportedFunction, _imports: &Imports) -> Result<()> {
        let op = match function.name() {
            "add" => Instruction::I32Add,
            "subtract" => Instruction::I32Sub,
            "multiply" => Instruction::I32Mul,
            "divide" => Instruction::I32DivS,
            other => anyhow::bail!("unexpected calculator function '{other}'"),
        };

        let body = function.body();
        body.emit(Instruction::LocalGet(0));
        body.emit(Instruction::LocalGet(1));
        body.emit(op);
        Ok(())
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
