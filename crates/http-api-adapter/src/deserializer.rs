//! The `WriteVisitor` that builds a WIT value from a JSON document through
//! json-mapper's `deserializer`, mapping each WIT name to the JSON name the
//! API uses.
//!
//! The names are mapped while emitting, so the generated component passes
//! the JSON names to json-mapper as literals, and json-mapper is unchanged.

use anyhow::{Result, anyhow};

use composable_factory::world::{
    ImportedFunction, ImportedInterface, MapKey, Type, Value, ValueSpec, WriteVisitor,
};

use crate::names::NameMap;

/// Produces a value from a JSON document via a `deserializer` resource handle.
pub struct JsonDeserializer {
    /// The `deserializer` interface.
    interface: ImportedInterface,
    /// The `deserializer` resource handle.
    handle: Value,
    /// The JSON names for the WIT names the walk reaches.
    names: NameMap,
    /// The key type of each map currently being read, innermost last. It
    /// indicates what form to expect: an object for string keys, an array of
    /// pairs otherwise. It also determines what accessor retrieves the key.
    open_map_keys: Vec<MapKey>,
    /// Which entry of the innermost map is being read, held across the whole
    /// entry, because an object reaches both its key and its value by index.
    entry_index: Option<Value>,
    /// Set while an object's key is being read, since the cursor cannot move
    /// onto a key. It is retrieved via `key-at` rather than a leaf accessor.
    reading_key: bool,
}

impl JsonDeserializer {
    /// Acquire the resource handle by calling the constructor with `json`, a
    /// string value holding the whole document.
    pub fn new(interface: ImportedInterface, json: Value, names: NameMap) -> Result<Self> {
        let handle = interface
            .function("[constructor]deserializer")?
            .call(&[json])?
            .ok_or_else(|| anyhow!("deserializer constructor must return a handle"))?;
        Ok(JsonDeserializer {
            interface,
            handle,
            names,
            open_map_keys: Vec::new(),
            entry_index: None,
            reading_key: false,
        })
    }

    /// Drop the resource handle.
    pub fn close(self) -> Result<()> {
        self.handle.drop()
    }

    /// A `u32` value holding `n`, for a visitor answer known while emitting.
    /// The factory API has no constant value, so this writes `n` into a fresh
    /// slot of `enter-element`'s `index` param, which is never called.
    pub fn constant(&self, n: u32) -> Result<Value> {
        let value = self.method("enter-element")?.param("index")?.value()?;
        value.write(&ValueSpec::u32(n))?;
        Ok(value)
    }

    /// A callable method for `[method]deserializer.<name>`.
    fn method(&self, name: &str) -> Result<ImportedFunction> {
        self.interface
            .function(&format!("[method]deserializer.{name}"))
    }

    /// Call a method taking the handle and an index, returning its result.
    fn call_with_index(&self, name: &str, index: &Value) -> Result<Value> {
        let h = self.handle.clone();
        self.method(name)?
            .call(&[h, index.clone()])?
            .ok_or_else(|| anyhow!("deserializer.{name} must return a value"))
    }

    /// Call a method taking the handle and a list of names, returning its
    /// result.
    fn call_with_names(&self, name: &str, names: &[String]) -> Result<Value> {
        let method = self.method(name)?;
        let argument = method.param("names")?.value()?;
        argument.write(&ValueSpec::list(names.iter().map(ValueSpec::string)))?;
        let h = self.handle.clone();
        method
            .call(&[h, argument])?
            .ok_or_else(|| anyhow!("deserializer.{name} must return a value"))
    }

    /// Pull one leaf via the named method.
    fn get(&mut self, name: &str) -> Result<ValueSpec> {
        Ok(self.pull(name)?.into())
    }

    /// Pull the value at the cursor via the named accessor.
    fn pull(&self, name: &str) -> Result<Value> {
        let h = self.handle.clone();
        self.method(name)?
            .call(&[h])?
            .ok_or_else(|| anyhow!("deserializer.{name} must return a value"))
    }

    /// Descend into the element at a runtime index.
    fn enter_element(&self, index: &Value) -> Result<()> {
        let h = self.handle.clone();
        self.method("enter-element")?.call(&[h, index.clone()])?;
        Ok(())
    }

    /// Descend into the element at an index known while emitting.
    fn enter_element_at(&self, index: u32) -> Result<()> {
        let enter = self.method("enter-element")?;
        let position = enter.param("index")?.value()?;
        position.write(&ValueSpec::u32(index))?;
        let h = self.handle.clone();
        enter.call(&[h, position])?;
        Ok(())
    }

    /// The key type of the map whose entry is being read.
    fn entry_key(&self) -> Result<MapKey> {
        self.open_map_keys
            .last()
            .copied()
            .ok_or_else(|| anyhow!("a map entry outside any map"))
    }

    /// Which entry of the innermost map is being read.
    fn entry_index(&self) -> Result<Value> {
        self.entry_index
            .clone()
            .ok_or_else(|| anyhow!("a map entry's member outside any entry"))
    }

    /// An object key index, if that is what's currently being read.
    fn reading_object_key(&self) -> Option<Value> {
        self.reading_key.then(|| self.entry_index.clone()).flatten()
    }

    /// Exit out of the current scope.
    fn exit(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("exit")?.call(&[h])?;
        Ok(())
    }
}

impl WriteVisitor for JsonDeserializer {
    /// The walk starts at the document itself, so nothing is entered and the
    /// value is always present.
    fn begin_walk(&mut self) -> Result<Value> {
        self.constant(1)
    }

    /// Nothing was entered, so no `exit` is owed.
    fn end_walk(&mut self) -> Result<()> {
        Ok(())
    }

    fn begin_field(&mut self, name: &str) -> Result<()> {
        let enter_field = self.method("enter-field")?;
        let key = enter_field.param("name")?.value()?;
        key.write(&ValueSpec::string(self.names.fields.apply(name)))?;
        let h = self.handle.clone();
        enter_field.call(&[h, key])?;
        Ok(())
    }

    fn end_field(&mut self) -> Result<()> {
        self.exit()
    }

    fn begin_element(&mut self, index: &Value) -> Result<()> {
        self.enter_element(index)
    }

    fn end_element(&mut self) -> Result<()> {
        self.exit()
    }

    // A map with string keys is read from a JSON object. A map with any other
    // key type is read from an array of [key, value] entry arrays. Keys are
    // data, not names, so they are never mapped.
    fn begin_map(&mut self, key: MapKey, _value: &Type) -> Result<()> {
        self.open_map_keys.push(key);
        Ok(())
    }

    fn end_map(&mut self) -> Result<()> {
        self.open_map_keys.pop();
        Ok(())
    }

    // An object's entry is reached by index, and its key sits beside its value
    // rather than within it, so `key-at` retrieves the key rather than
    // descending into the entry. Every other map is an array of [key, value]
    // arrays, so its entry is descended into like any other element.
    fn begin_entry(&mut self, index: &Value) -> Result<()> {
        self.entry_index = Some(index.clone());
        if self.entry_key()? != MapKey::String {
            self.enter_element(index)?;
        }
        Ok(())
    }

    fn end_entry(&mut self) -> Result<()> {
        let string_keyed = self.entry_key()? == MapKey::String;
        self.entry_index = None;
        if string_keyed {
            return Ok(());
        }
        self.exit()
    }

    fn begin_key(&mut self) -> Result<()> {
        // An object's key is retrieved via `key-at`, by entry index, so the
        // cursor is not moved and `on_string` handles the index lookup.
        if self.entry_key()? == MapKey::String {
            self.reading_key = true;
            return Ok(());
        }
        self.enter_element_at(0)
    }

    fn end_key(&mut self) -> Result<()> {
        if self.entry_key()? == MapKey::String {
            self.reading_key = false;
            return Ok(());
        }
        self.exit()
    }

    fn begin_value(&mut self) -> Result<()> {
        match self.entry_key()? {
            // The object's `index`th value, since its key was read in place.
            MapKey::String => {
                let index = self.entry_index()?;
                self.enter_element(&index)
            }
            _ => self.enter_element_at(1),
        }
    }

    fn end_value(&mut self) -> Result<()> {
        self.exit()
    }

    fn begin_payload(&mut self) -> Result<()> {
        let h = self.handle.clone();
        self.method("enter-payload")?.call(&[h])?;
        Ok(())
    }

    fn end_payload(&mut self) -> Result<()> {
        self.exit()
    }

    fn length(&mut self) -> Result<Value> {
        let h = self.handle.clone();
        self.method("length")?
            .call(&[h])?
            .ok_or_else(|| anyhow!("deserializer.length must return a count"))
    }

    /// The case names are mapped by the enum rules, so an enum's value is
    /// matched as the API spells it. A value not among them traps.
    fn case_index(&mut self, names: &[&str]) -> Result<Value> {
        let mapped = self.names.enums.apply_all(names)?;
        self.call_with_names("case-index", &mapped)?
            .assert_case("some")
    }

    /// Asks the mapper whether there is a value at the cursor, for an option.
    fn has_value(&mut self) -> Result<Value> {
        self.pull("has-value")
    }

    /// Asks the mapper which of the record's fields the JSON object holds, by
    /// their JSON names, in the order the record declares them.
    fn field_presence(&mut self, names: &[&str]) -> Result<Value> {
        let mapped = self.names.fields.apply_all(names)?;
        self.call_with_names("field-presence", &mapped)
    }

    /// Asks the mapper which flags are set, by their JSON names. A set flag
    /// the type does not declare has no bit, so the component traps rather
    /// than silently dropping it.
    fn on_flags(&mut self, declared: &[String]) -> Result<ValueSpec> {
        let declared: Vec<&str> = declared.iter().map(String::as_str).collect();
        let mapped = self.names.enums.apply_all(&declared)?;
        let found = self.call_with_names("flag-bits", &mapped)?;
        ValueSpec::flag_bits(found.assert_case("ok")?)
    }

    fn on_string(&mut self) -> Result<ValueSpec> {
        // An object's key is retrieved via `key-at`, by entry index, rather
        // than by a leaf accessor reading the cursor.
        if let Some(index) = self.reading_object_key() {
            return Ok(self.call_with_index("key-at", &index)?.into());
        }
        self.get("get-string")
    }
    fn on_char(&mut self) -> Result<ValueSpec> {
        self.get("get-char")
    }
    fn on_bool(&mut self) -> Result<ValueSpec> {
        self.get("get-bool")
    }
    fn on_f32(&mut self) -> Result<ValueSpec> {
        self.get("get-f32")
    }
    fn on_f64(&mut self) -> Result<ValueSpec> {
        self.get("get-f64")
    }
    fn on_s8(&mut self) -> Result<ValueSpec> {
        self.get("get-s8")
    }
    fn on_s16(&mut self) -> Result<ValueSpec> {
        self.get("get-s16")
    }
    fn on_s32(&mut self) -> Result<ValueSpec> {
        self.get("get-s32")
    }
    fn on_s64(&mut self) -> Result<ValueSpec> {
        self.get("get-s64")
    }
    fn on_u8(&mut self) -> Result<ValueSpec> {
        self.get("get-u8")
    }
    fn on_u16(&mut self) -> Result<ValueSpec> {
        self.get("get-u16")
    }
    fn on_u32(&mut self) -> Result<ValueSpec> {
        self.get("get-u32")
    }
    fn on_u64(&mut self) -> Result<ValueSpec> {
        self.get("get-u64")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use composable_factory::wit::PackageSource;
    use composable_factory::world::{ExportedFunction, Imports};
    use composable_factory::{ComponentBuilder, World};

    use crate::names::{Convention, Rules};

    /// The json-mapper interfaces, as vendored in this crate's `wit`.
    const MAPPER_WIT: &str = include_str!("../wit/mapper.wit");

    /// A projection of a weather.gov forecast: the document's nesting as
    /// records, with every leaf kind the walk reaches through a name.
    const FORECAST_WIT: &str = r"package test:forecast;
        interface forecast {
          enum severity { extreme, severe, minor }
          flags hazards { heavy-rain, high-wind }
          record quantity { unit-code: string, value: option<f64> }
          record period {
            id: string,
            number: u32,
            start-time: string,
            is-daytime: bool,
            temperature: s32,
            short-forecast: string,
            probability-of-precipitation: option<quantity>,
            severity: severity,
            hazards: hazards,
            labels: map<string, string>,
          }
          record forecast-properties { periods: list<period> }
          record forecast { properties: forecast-properties }
          read: async func(body: string) -> forecast;
        }
        world w { export forecast; }";

    /// Reads its export's result from the `body` param through
    /// [`JsonDeserializer`] with `names`.
    struct Reader {
        names: NameMap,
    }

    impl ComponentBuilder for Reader {
        fn build_world(&self, world: &mut World) -> Result<()> {
            world.add_imports(PackageSource::from_text(MAPPER_WIT)?.interface("deserializer")?)?;
            world.add_exports(PackageSource::from_text(FORECAST_WIT)?.interface("forecast")?)
        }

        fn build_function(&self, function: &ExportedFunction, imports: &Imports) -> Result<()> {
            let body = function.param("body")?.receive()?;
            let mut deserializer = JsonDeserializer::new(
                imports.interface("deserializer")?,
                body,
                self.names.clone(),
            )?;
            function
                .result()
                .ok_or_else(|| anyhow!("read returns a forecast"))?
                .value()
                .write_with(&mut deserializer)?;
            deserializer.close()
        }
    }

    /// weather.gov's spelling: camelCase fields, PascalCase values, and a
    /// JSON-LD key no convention produces.
    fn weather_gov() -> NameMap {
        NameMap {
            fields: Rules::new(Convention::Camel).with_override("id", "@id"),
            enums: Rules::new(Convention::Pascal),
        }
    }

    /// Whether `bytes` contain `text`.
    fn contains(bytes: &[u8], text: &str) -> bool {
        bytes
            .windows(text.len())
            .any(|window| window == text.as_bytes())
    }

    #[test]
    fn a_nested_projection_builds_with_mapped_names() {
        let bytes = composable_factory::build(&Reader {
            names: weather_gov(),
        })
        .expect("a projection must build");
        // The WIT names are in the component's type, so only the JSON names
        // show that the mapping reached the generated code.
        for json in [
            "@id",
            "startTime",
            "isDaytime",
            "shortForecast",
            "probabilityOfPrecipitation",
            "unitCode",
            "Severe",
            "HeavyRain",
        ] {
            assert!(contains(&bytes, json), "no '{json}' in the component");
        }
    }

    #[test]
    fn a_name_collision_fails_the_build() {
        let error = composable_factory::build(&Reader {
            names: NameMap {
                fields: Rules::new(Convention::Camel).with_override("number", "startTime"),
                enums: Rules::new(Convention::Pascal),
            },
        })
        .expect_err("two fields map to startTime");
        assert!(
            format!("{error:#}")
                .contains("'number' and 'start-time' both map to the JSON name 'startTime'"),
            "{error:#}"
        );
    }
}
