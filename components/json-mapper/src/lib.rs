wit_bindgen::generate!({
    path: "wit",
    world: "json-mapper",
    generate_all,
});

use std::cell::RefCell;

use exports::composable::factory::deserializer::{Guest as DeserializerGuest, GuestDeserializer};
use exports::composable::factory::serializer::{Guest as SerializerGuest, GuestSerializer};

struct Mapper;

impl SerializerGuest for Mapper {
    type Serializer = Serializer;
}

impl DeserializerGuest for Mapper {
    type Deserializer = Deserializer;
}

/// Incremental JSON reader.
pub struct Deserializer {
    // The cursor: `stack[0]` is the root document. The last entry is the
    // caller's current location. Each successful `enter-*` pushes, and `exit`
    // pops. Depth therefore always mirrors the caller's recursion depth.
    stack: RefCell<Vec<serde_json::Value>>,
}

impl GuestDeserializer for Deserializer {
    fn new(input: String) -> Self {
        let root = serde_json::from_str(&input).unwrap_or(serde_json::Value::Null);
        Deserializer {
            stack: RefCell::new(vec![root]),
        }
    }

    fn field_presence(&self, names: Vec<String>) -> u32 {
        let stack = self.stack.borrow();
        let Some(serde_json::Value::Object(o)) = stack.last() else {
            // Not an object, so it supplies no fields.
            return 0;
        };
        let mut bits = 0u32;
        for (index, name) in names.iter().enumerate().take(32) {
            if o.contains_key(name) {
                bits |= 1 << index;
            }
        }
        bits
    }

    fn enter_field(&self, name: String) {
        let child = self
            .stack
            .borrow()
            .last()
            .and_then(|v| v.get(&name))
            .cloned();
        // Only a field `field-presence` reported should be entered, so this
        // indicates an invalid reader, and traps.
        let Some(child) = child else {
            panic!("enter-field: no field '{name}' after reporting it present");
        };
        self.stack.borrow_mut().push(child);
    }

    // A string-keyed map arrives as an object, every other sequence as an
    // array. Both are counted and indexed here.
    fn length(&self) -> u32 {
        match self.stack.borrow().last() {
            Some(serde_json::Value::Array(a)) => a.len() as u32,
            Some(serde_json::Value::Object(o)) => o.len() as u32,
            _ => 0,
        }
    }

    fn enter_element(&self, index: u32) -> bool {
        let child = match self.stack.borrow().last() {
            Some(serde_json::Value::Array(a)) => a.get(index as usize).cloned(),
            // An object's values are retrieved by index, its keys by `key-at`.
            Some(serde_json::Value::Object(o)) => o.values().nth(index as usize).cloned(),
            _ => None,
        };
        match child {
            Some(v) => {
                self.stack.borrow_mut().push(v);
                true
            }
            None => false,
        }
    }

    fn key_at(&self, index: u32) -> String {
        // `keys` and `values` iterate one map in the same order, so the nth
        // key pairs with the nth value `enter-element` returns.
        match self.stack.borrow().last() {
            Some(serde_json::Value::Object(o)) => {
                o.keys().nth(index as usize).cloned().unwrap_or_default()
            }
            _ => String::new(),
        }
    }

    fn case_index(&self, names: Vec<String>) -> Option<u32> {
        // A variant is `{"type": <name>, "value": <payload>}`.
        // An enum is a bare string (what `serializer.add-enum` writes).
        let active = match self.stack.borrow().last() {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.get("type").and_then(|c| c.as_str())?.to_string(),
            None => return None,
        };
        // The caller supplied the names, so an index identifies the case.
        // Keeps this string comparison here, not emitted into wasm.
        names.iter().position(|n| n == &active).map(|i| i as u32)
    }

    fn enter_payload(&self) {
        let child = self
            .stack
            .borrow()
            .last()
            .and_then(|v| v.get("value"))
            .cloned();
        // A unit case should never be entered: the caller knows from the case
        // this reader named in `case-index` whether it carries a payload. So a
        // missing `value` means this is an invalid reader, and traps.
        let Some(child) = child else {
            panic!("enter-payload: the case named by case-index has no 'value'");
        };
        self.stack.borrow_mut().push(child);
    }

    fn flag_bits(&self, declared_names: Vec<String>) -> Result<u32, String> {
        // The component model supports a max of 32 flags.
        if declared_names.len() > 32 {
            return Err(format!("at most 32 flags, got {}", declared_names.len()));
        }
        let mut bits = 0u32;
        let stack = self.stack.borrow();
        let Some(serde_json::Value::Array(set_names)) = stack.last() else {
            return Ok(bits);
        };
        // Each flag's bit position matches the index of its declared name.
        for name in set_names.iter().filter_map(|f| f.as_str()) {
            let index = declared_names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| format!("no flag '{name}'"))?;
            bits |= 1u32 << index;
        }
        Ok(bits)
    }

    fn exit(&self) {
        let mut stack = self.stack.borrow_mut();
        // Never pop the root, so an unbalanced `exit` leaves the cursor at the
        // document rather than emptying the stack.
        if stack.len() > 1 {
            stack.pop();
        }
    }

    fn get_string(&self) -> String {
        match self.stack.borrow().last() {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => String::new(),
        }
    }

    fn get_char(&self) -> char {
        // A JSON string's first char, else the null char.
        match self.stack.borrow().last() {
            Some(serde_json::Value::String(s)) => s.chars().next().unwrap_or('\0'),
            _ => '\0',
        }
    }

    fn get_bool(&self) -> bool {
        matches!(
            self.stack.borrow().last(),
            Some(serde_json::Value::Bool(true))
        )
    }

    fn get_s8(&self) -> i8 {
        self.as_i64() as i8
    }
    fn get_s16(&self) -> i16 {
        self.as_i64() as i16
    }
    fn get_s32(&self) -> i32 {
        self.as_i64() as i32
    }
    fn get_s64(&self) -> i64 {
        self.as_i64()
    }
    fn get_u8(&self) -> u8 {
        self.as_u64() as u8
    }
    fn get_u16(&self) -> u16 {
        self.as_u64() as u16
    }
    fn get_u32(&self) -> u32 {
        self.as_u64() as u32
    }
    fn get_u64(&self) -> u64 {
        self.as_u64()
    }
    fn get_f32(&self) -> f32 {
        self.as_f64() as f32
    }
    fn get_f64(&self) -> f64 {
        self.as_f64()
    }
}

impl Deserializer {
    fn as_i64(&self) -> i64 {
        self.stack
            .borrow()
            .last()
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }
    fn as_u64(&self) -> u64 {
        self.stack
            .borrow()
            .last()
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }
    fn as_f64(&self) -> f64 {
        self.stack
            .borrow()
            .last()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
    }
}

/// Incremental JSON writer.
pub struct Serializer {
    inner: RefCell<Inner>,
}

struct Inner {
    out: String,
    // Open containers, innermost last. Each counts its members so
    // `before_value` knows whether a comma is needed.
    stack: Vec<Frame>,
    // Set by `add-key` to indicate the next value is for this field.
    pending_key: bool,
}

enum Frame {
    Object { count: usize },
    Array { count: usize },
}

impl Serializer {
    fn new() -> Self {
        Serializer {
            inner: RefCell::new(Inner {
                out: String::new(),
                stack: Vec::new(),
                pending_key: false,
            }),
        }
    }
}

impl Inner {
    // Emit separators and handle positioning before writing a value token.
    fn before_value(&mut self) {
        if self.pending_key {
            // Key was written with its ':', so value follows with no comma.
            self.pending_key = false;
            return;
        }
        let mut need_comma = false;
        if let Some(Frame::Array { count }) = self.stack.last_mut() {
            // Only array elements after the first need commas.
            need_comma = *count > 0;
            *count += 1;
        }
        if need_comma {
            self.out.push(',');
        }
    }

    fn write_str_literal(&mut self, s: &str) {
        self.out.push('"');
        for c in s.chars() {
            match c {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                // All other control chars take `\uXXXX` form.
                c if (c as u32) < 0x20 => self.out.push_str(&format!("\\u{:04x}", c as u32)),
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }
}

macro_rules! add_num {
    ($method:ident, $ty:ty) => {
        fn $method(&self, n: $ty) {
            let mut i = self.inner.borrow_mut();
            i.before_value();
            i.out.push_str(&n.to_string());
        }
    };
}

impl GuestSerializer for Serializer {
    fn new() -> Self {
        Serializer::new()
    }

    fn add_bool(&self, b: bool) {
        let mut i = self.inner.borrow_mut();
        i.before_value();
        i.out.push_str(if b { "true" } else { "false" });
    }

    add_num!(add_s8, i8);
    add_num!(add_s16, i16);
    add_num!(add_s32, i32);
    add_num!(add_s64, i64);
    add_num!(add_u8, u8);
    add_num!(add_u16, u16);
    add_num!(add_u32, u32);
    add_num!(add_u64, u64);
    add_num!(add_f32, f32);
    add_num!(add_f64, f64);

    fn add_char(&self, c: char) {
        let mut i = self.inner.borrow_mut();
        i.before_value();
        let s = c.to_string();
        i.write_str_literal(&s);
    }

    fn add_string(&self, s: String) {
        let mut i = self.inner.borrow_mut();
        i.before_value();
        i.write_str_literal(&s);
    }

    fn add_null(&self) {
        let mut i = self.inner.borrow_mut();
        i.before_value();
        i.out.push_str("null");
    }

    fn begin_object(&self) {
        let mut i = self.inner.borrow_mut();
        i.before_value();
        i.out.push('{');
        i.stack.push(Frame::Object { count: 0 });
    }

    fn add_key(&self, name: String) {
        let mut i = self.inner.borrow_mut();
        let mut need_comma = false;
        if let Some(Frame::Object { count }) = i.stack.last_mut() {
            need_comma = *count > 0;
            *count += 1;
        }
        if need_comma {
            i.out.push(',');
        }
        i.write_str_literal(&name);
        i.out.push(':');
        i.pending_key = true;
    }

    fn end_object(&self) {
        let mut i = self.inner.borrow_mut();
        i.stack.pop();
        i.out.push('}');
    }

    fn begin_array(&self) {
        let mut i = self.inner.borrow_mut();
        i.before_value();
        i.out.push('[');
        i.stack.push(Frame::Array { count: 0 });
    }

    fn end_array(&self) {
        let mut i = self.inner.borrow_mut();
        i.stack.pop();
        i.out.push(']');
    }

    fn add_enum(&self, case: String) {
        self.add_string(case);
    }

    // {"type": <name>}, with no "value" key since no payload follows.
    fn add_case(&self, case: String) {
        self.begin_object();
        self.add_key("type".to_string());
        self.add_string(case);
        self.end_object();
    }

    // {"type": <name>, "value": <payload>}, closed by `end_case`.
    fn begin_case(&self, case: String) {
        self.begin_object();
        self.add_key("type".to_string());
        self.add_string(case);
        self.add_key("value".to_string());
    }

    fn end_case(&self) {
        self.end_object();
    }

    fn begin_flags(&self) {
        self.begin_array();
    }

    fn add_flag(&self, name: String) {
        self.add_string(name);
    }

    fn end_flags(&self) {
        self.end_array();
    }

    fn finish(&self) -> String {
        self.inner.borrow().out.clone()
    }
}

export!(Mapper);
