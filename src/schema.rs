//! WIT types as JSON Schema, traversing the navigable World.

use serde_json::{Value, json};

use crate::world::{Kind, Param, Type};

/// A WIT type as JSON Schema.
pub fn schema(ty: Type) -> Value {
    match ty.kind() {
        Kind::Bool => json!({"type": "boolean"}),
        Kind::U8 => json!({"type": "number", "minimum": 0, "maximum": 255}),
        Kind::U16 => json!({"type": "number", "minimum": 0, "maximum": 65535}),
        Kind::U32 => json!({"type": "number", "minimum": 0, "maximum": 4294967295_u64}),
        // No maximum: u64 exceeds exact JSON number representation.
        Kind::U64 => json!({"type": "number", "minimum": 0}),
        Kind::S8 => json!({"type": "number", "minimum": -128, "maximum": 127}),
        Kind::S16 => json!({"type": "number", "minimum": -32768, "maximum": 32767}),
        Kind::S32 => json!({"type": "number", "minimum": -2147483648_i64, "maximum": 2147483647}),
        // Unbounded: s64 exceeds exact JSON number representation at both ends.
        Kind::S64 => json!({"type": "number"}),
        Kind::F32 | Kind::F64 => json!({"type": "number"}),
        Kind::Char => json!({"type": "string", "minLength": 1, "maxLength": 1}),
        Kind::String => json!({"type": "string"}),
        Kind::ErrorContext => json!({"type": "string"}),

        Kind::Record(fields) => {
            let mut properties = serde_json::Map::new();
            let mut required = Vec::new();
            for field in &fields {
                properties.insert(field.name().to_string(), schema(field.ty()));
                if !is_optional(field.ty()) {
                    required.push(field.name().to_string());
                }
            }
            let mut object = json!({
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false
            });
            // The declared name becomes the title. `name()` follows aliases,
            // so this is the bare name the WIT declares.
            if let Some(name) = ty.name() {
                object["title"] = json!(name);
            }
            object
        }

        // A variant is a tagged object: the case name under `type`, and the
        // payload under `value` when the case carries one.
        Kind::Variant(cases) => {
            let cases: Vec<Value> = cases
                .into_iter()
                .map(|case| match case.payload() {
                    Some(payload) => json!({
                        "type": "object",
                        "properties": {
                            "type": {"const": case.name()},
                            "value": schema(payload)
                        },
                        "required": ["type", "value"],
                        "additionalProperties": false
                    }),
                    None => json!({
                        "type": "object",
                        "properties": { "type": {"const": case.name()} },
                        "required": ["type"],
                        "additionalProperties": false
                    }),
                })
                .collect();
            json!({ "oneOf": cases })
        }

        Kind::Enum(cases) => json!({ "type": "string", "enum": cases }),

        Kind::Option(inner) => json!({ "oneOf": [ schema(inner), {"type": "null"} ] }),

        Kind::Result { ok, err } => {
            let ok = ok.map(schema).unwrap_or_else(|| json!({"type": "null"}));
            let err = err.map(schema).unwrap_or_else(|| json!({"type": "null"}));
            json!({
                "oneOf": [
                    {
                        "type": "object",
                        "properties": { "ok": ok },
                        "required": ["ok"],
                        "additionalProperties": false
                    },
                    {
                        "type": "object",
                        "properties": { "error": err },
                        "required": ["error"],
                        "additionalProperties": false
                    }
                ]
            })
        }

        Kind::List(elem) => json!({ "type": "array", "items": schema(elem) }),

        Kind::FixedLengthList(elem, count) => json!({
            "type": "array",
            "items": schema(elem),
            "minItems": count,
            "maxItems": count
        }),

        Kind::Map(key, value) => {
            let value = schema(value);
            // A string-keyed map is a JSON object since JSON keys are strings.
            // A map with any other key type is represented as a list of pairs.
            if matches!(key.kind(), Kind::String) {
                json!({ "type": "object", "additionalProperties": value })
            } else {
                json!({
                    "type": "array",
                    "items": {
                        "type": "array",
                        "prefixItems": [schema(key), value],
                        "minItems": 2,
                        "maxItems": 2
                    }
                })
            }
        }

        Kind::Tuple(members) => {
            let items: Vec<Value> = members.into_iter().map(schema).collect();
            let count = items.len();
            json!({
                "type": "array",
                "prefixItems": items,
                "minItems": count,
                "maxItems": count
            })
        }

        Kind::Flags(names) => json!({
            "type": "array",
            "items": { "type": "string", "enum": names },
            "uniqueItems": true
        }),

        // No JSON Schema `type` for these, so `x-wit-type` names them instead.
        Kind::Resource | Kind::Handle(_) => json!({
            "x-wit-type": "resource",
            "description": "Resource handle (no JSON value representation)"
        }),
        Kind::Future(_) => json!({
            "x-wit-type": "future",
            "description": "Future (no JSON value representation)"
        }),
        Kind::Stream(_) => json!({
            "x-wit-type": "stream",
            "description": "Stream (no JSON value representation)"
        }),
    }
}

/// Whether a value of this type may be absent, in which case it will not be
/// included in an object's `required` list.
fn is_optional(ty: Type) -> bool {
    matches!(ty.kind(), Kind::Option(_))
}

/// A function's params as one object schema.
pub fn input_schema(params: &[impl Param]) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for param in params {
        properties.insert(param.name().to_string(), schema(param.ty()));
        if !is_optional(param.ty()) {
            required.push(param.name().to_string());
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// A function's result as a schema, or `null` if it has no return.
pub fn output_schema(result: Option<Type>) -> Value {
    match result {
        Some(ty) => schema(ty),
        None => json!({"type": "null"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::BuildContext;
    use std::rc::Rc;
    use wit_parser::Resolve;

    /// The schema of a named type declared in `wit`.
    fn schema_of(wit: &str, name: &str) -> Value {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("test.wit", wit).expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");
        let id = resolve.worlds[world]
            .imports
            .values()
            .find_map(|item| match item {
                wit_parser::WorldItem::Interface { id, .. } => {
                    resolve.interfaces[*id].types.get(name).copied()
                }
                _ => None,
            })
            .expect("the declared type");
        let ctx = Rc::new(BuildContext::new(Rc::new(resolve), world));
        schema(Type::new(ctx, wit_parser::Type::Id(id)))
    }

    /// A world declaring `types` inside one interface.
    fn wit(types: &str) -> String {
        format!(
            r"package test:schema;
              interface i {{ {types} f: func(a: u32); }}
              world w {{ import i; }}"
        )
    }

    #[test]
    fn integers_carry_the_bounds_of_their_width() {
        let source = wit("type small = u8; type wide = s32;");
        assert_eq!(
            schema_of(&source, "small"),
            json!({"type": "number", "minimum": 0, "maximum": 255})
        );
        assert_eq!(
            schema_of(&source, "wide"),
            json!({"type": "number", "minimum": -2147483648_i64, "maximum": 2147483647})
        );
    }

    #[test]
    fn a_record_lists_its_required_fields() {
        let source = wit("record point { x: u32, y: u32 }");
        let schema = schema_of(&source, "point");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["title"], "point");
        assert_eq!(schema["required"], json!(["x", "y"]));
        assert_eq!(schema["additionalProperties"], json!(false));
    }

    #[test]
    fn an_optional_field_is_not_required() {
        // A field that may be absent must not appear in `required`.
        let source = wit("record person { name: string, nickname: option<string> }");
        let schema = schema_of(&source, "person");
        assert_eq!(schema["required"], json!(["name"]));
        assert!(schema["properties"]["nickname"]["oneOf"].is_array());
    }

    #[test]
    fn a_variant_becomes_a_tagged_one_of() {
        let source = wit("variant shape { circle(u32), empty }");
        let schema = schema_of(&source, "shape");
        let cases = schema["oneOf"].as_array().expect("one per case");
        assert_eq!(cases.len(), 2);
        // The case carrying a payload requires both keys.
        assert_eq!(cases[0]["properties"]["type"]["const"], "circle");
        assert_eq!(cases[0]["required"], json!(["type", "value"]));
        // The unit case requires only the tag.
        assert_eq!(cases[1]["properties"]["type"]["const"], "empty");
        assert_eq!(cases[1]["required"], json!(["type"]));
    }

    #[test]
    fn an_enum_is_a_string_over_its_case_names() {
        let source = wit("enum color { red, green }");
        assert_eq!(
            schema_of(&source, "color"),
            json!({"type": "string", "enum": ["red", "green"]})
        );
    }

    #[test]
    fn a_result_is_ok_or_error() {
        let source = wit("type outcome = result<u32, string>;");
        let schema = schema_of(&source, "outcome");
        let cases = schema["oneOf"].as_array().expect("two cases");
        assert_eq!(cases[0]["required"], json!(["ok"]));
        assert_eq!(cases[1]["required"], json!(["error"]));
    }

    #[test]
    fn a_result_with_no_payload_is_null() {
        let source = wit("type done = result;");
        let schema = schema_of(&source, "done");
        let cases = schema["oneOf"].as_array().expect("two cases");
        assert_eq!(cases[0]["properties"]["ok"], json!({"type": "null"}));
        assert_eq!(cases[1]["properties"]["error"], json!({"type": "null"}));
    }

    #[test]
    fn a_list_is_an_array_with_its_element_schema() {
        let source = wit("type numbers = list<u32>;");
        assert_eq!(
            schema_of(&source, "numbers"),
            json!({
                "type": "array",
                "items": {"type": "number", "minimum": 0, "maximum": 4294967295_u64}
            })
        );
    }

    #[test]
    fn a_fixed_length_list_pins_its_bounds() {
        let source = wit("type triple = list<u32, 3>;");
        let schema = schema_of(&source, "triple");
        assert_eq!(schema["minItems"], 3);
        assert_eq!(schema["maxItems"], 3);
    }

    #[test]
    fn a_string_keyed_map_is_an_object() {
        let source = wit("type table = map<string, u32>;");
        assert_eq!(
            schema_of(&source, "table"),
            json!({
                "type": "object",
                "additionalProperties": {
                    "type": "number", "minimum": 0, "maximum": 4294967295_u64
                }
            })
        );
    }

    #[test]
    fn a_map_with_a_non_string_key_is_a_list_of_pairs() {
        // JSON object keys are strings, so anything else has to be positional.
        let source = wit("type table = map<u32, string>;");
        let schema = schema_of(&source, "table");
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"]["minItems"], 2);
        assert_eq!(schema["items"]["prefixItems"][0]["type"], "number");
        assert_eq!(schema["items"]["prefixItems"][1]["type"], "string");
    }

    #[test]
    fn a_tuple_pins_its_member_count() {
        let source = wit("type pair = tuple<u32, string>;");
        let schema = schema_of(&source, "pair");
        assert_eq!(schema["minItems"], 2);
        assert_eq!(schema["maxItems"], 2);
        assert_eq!(schema["prefixItems"][1]["type"], "string");
    }

    #[test]
    fn flags_are_a_set_of_declared_names() {
        let source = wit("flags perms { read, write }");
        let schema = schema_of(&source, "perms");
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["uniqueItems"], json!(true));
        assert_eq!(schema["items"]["enum"], json!(["read", "write"]));
    }

    #[test]
    fn a_resource_is_named_by_an_extension_key() {
        // No JSON Schema `type` describes a handle, so none is emitted.
        let source = wit("resource handle {}");
        let schema = schema_of(&source, "handle");
        assert_eq!(schema["x-wit-type"], "resource");
        assert!(schema.get("type").is_none());
    }

    #[test]
    fn futures_and_streams_are_named_by_an_extension_key() {
        for (declared, expected) in [
            ("type pending = future<u32>;", "future"),
            ("type bytes = stream<u8>;", "stream"),
        ] {
            let source = wit(declared);
            let name = declared.split_whitespace().nth(1).expect("the type name");
            let schema = schema_of(&source, name);
            assert_eq!(schema["x-wit-type"], expected);
            assert!(schema.get("type").is_none(), "{declared}");
        }
    }

    #[test]
    fn a_function_with_no_result_outputs_null() {
        assert_eq!(output_schema(None), json!({"type": "null"}));
    }

    /// The params of a single function of `wit`'s single interface.
    fn params_of(wit: &str) -> Vec<crate::world::ImportedFunctionParam> {
        let mut resolve = Resolve::new();
        let package = resolve.push_str("test.wit", wit).expect("parse");
        let world = resolve.select_world(&[package], None).expect("one world");
        let ctx = Rc::new(BuildContext::new(Rc::new(resolve), world));
        crate::world::Imports::new(ctx, crate::emitter::Emitter::new(0))
            .interface("i")
            .expect("the interface")
            .function("f")
            .expect("the function")
            .params()
    }

    #[test]
    fn params_become_one_object_of_required_properties() {
        let params = params_of(
            r"package test:inputs;
              interface i { f: func(name: string, count: u32); }
              world w { import i; }",
        );
        assert_eq!(
            input_schema(&params),
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "count": {"type": "number", "minimum": 0, "maximum": 4294967295_u64}
                },
                "required": ["name", "count"],
                "additionalProperties": false
            })
        );
    }

    #[test]
    fn an_optional_param_is_not_required() {
        let params = params_of(
            r"package test:optionalinput;
              interface i { f: func(name: string, nickname: option<string>); }
              world w { import i; }",
        );
        assert_eq!(input_schema(&params)["required"], json!(["name"]));
    }
}
