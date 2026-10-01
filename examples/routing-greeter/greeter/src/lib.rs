//! A greeter whose greeting is its `greeting` config value, or `hello`.

wit_bindgen::generate!({
    path: "../wit",
    world: "language-greeter",
    generate_all,
});

struct Greeter;

impl exports::example::routing_greeter::greeter::Guest for Greeter {
    fn greet(name: String) -> String {
        let greeting = wasi::config::store::get("greeting")
            .ok()
            .flatten()
            .unwrap_or_else(|| "hello".to_string());
        format!("{greeting} {name}!")
    }
}

export!(Greeter);
