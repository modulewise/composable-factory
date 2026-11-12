//! The `WriteVisitor` that builds an endpoint's `result<T, http-error>` from a
//! successful response's JSON body.
//!
//! The factory API cannot write the `ok` case and hand back its payload to
//! walk on its own, so the walk starts at the result. It asks which case
//! applies and emits both cases' payloads. This visitor answers `ok`, reads
//! `T` through the JSON visitor, and answers the `err` payload, which never
//! runs, with constants.

use anyhow::{Result, bail};

use composable_factory::world::{MapKey, Type, Value, ValueSpec, WriteVisitor};

use crate::deserializer::JsonDeserializer;

/// Where the walk is within the result.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Position {
    /// At the result itself, before either case.
    Result,
    /// Within the `ok` payload, which is read from JSON.
    Ok,
    /// Between the `ok` and `err` payloads.
    Between,
    /// Within the `err` payload, which is never written at runtime.
    Err,
    Done,
}

/// Builds `result<T, http-error>` as `ok`, with `T` read from JSON.
pub struct ResultReader {
    json: JsonDeserializer,
    position: Position,
    /// Payloads entered within the current case's payload, such as an
    /// `option` in `T`, so their ends are not taken for the case's own.
    depth: usize,
}

impl ResultReader {
    pub fn new(json: JsonDeserializer) -> Self {
        ResultReader {
            json,
            position: Position::Result,
            depth: 0,
        }
    }

    /// The JSON visitor, to close once the walk is done.
    pub fn into_inner(self) -> JsonDeserializer {
        self.json
    }

    /// The JSON visitor, for content within the `ok` payload.
    fn json(&mut self) -> Result<&mut JsonDeserializer> {
        match self.position {
            Position::Ok => Ok(&mut self.json),
            other => bail!("the result walk reached JSON content at {other:?}"),
        }
    }

    fn in_err(&self) -> bool {
        self.position == Position::Err
    }
}

impl WriteVisitor for ResultReader {
    fn begin_walk(&mut self) -> Result<Value> {
        self.json.begin_walk()
    }

    fn end_walk(&mut self) -> Result<()> {
        self.json.end_walk()
    }

    /// `ok` (case 0) for the result itself. Within the `err` payload, the
    /// case is irrelevant.
    fn case_index(&mut self, names: &[&str]) -> Result<Value> {
        match self.position {
            Position::Result | Position::Err => self.json.constant(0),
            _ => self.json()?.case_index(names),
        }
    }

    fn begin_payload(&mut self) -> Result<()> {
        match self.position {
            Position::Result => self.position = Position::Ok,
            Position::Between => self.position = Position::Err,
            Position::Ok => {
                self.depth += 1;
                self.json.begin_payload()?;
            }
            Position::Err => self.depth += 1,
            Position::Done => bail!("the result walk entered a payload after both cases"),
        }
        Ok(())
    }

    fn end_payload(&mut self) -> Result<()> {
        match (self.position, self.depth) {
            (Position::Ok, 0) => self.position = Position::Between,
            (Position::Ok, _) => {
                self.depth -= 1;
                self.json.end_payload()?;
            }
            (Position::Err, 0) => self.position = Position::Done,
            (Position::Err, _) => self.depth -= 1,
            (other, _) => bail!("the result walk ended a payload at {other:?}"),
        }
        Ok(())
    }

    fn field_presence(&mut self, names: &[&str]) -> Result<Value> {
        match self.in_err() {
            true => self.json.constant(0),
            false => self.json()?.field_presence(names),
        }
    }

    fn begin_field(&mut self, name: &str) -> Result<()> {
        match self.in_err() {
            true => Ok(()),
            false => self.json()?.begin_field(name),
        }
    }

    fn end_field(&mut self) -> Result<()> {
        match self.in_err() {
            true => Ok(()),
            false => self.json()?.end_field(),
        }
    }

    fn has_value(&mut self) -> Result<Value> {
        match self.in_err() {
            true => self.json.constant(0),
            false => self.json()?.has_value(),
        }
    }

    fn length(&mut self) -> Result<Value> {
        match self.in_err() {
            true => self.json.constant(0),
            false => self.json()?.length(),
        }
    }

    fn on_string(&mut self) -> Result<ValueSpec> {
        match self.in_err() {
            true => Ok(ValueSpec::string("")),
            false => self.json()?.on_string(),
        }
    }

    fn on_u16(&mut self) -> Result<ValueSpec> {
        match self.in_err() {
            true => Ok(ValueSpec::u16(0)),
            false => self.json()?.on_u16(),
        }
    }

    // Everything else only occurs within `T`.

    fn begin_element(&mut self, index: &Value) -> Result<()> {
        self.json()?.begin_element(index)
    }
    fn end_element(&mut self) -> Result<()> {
        self.json()?.end_element()
    }
    fn begin_map(&mut self, key: MapKey, value: &Type) -> Result<()> {
        self.json()?.begin_map(key, value)
    }
    fn end_map(&mut self) -> Result<()> {
        self.json()?.end_map()
    }
    fn begin_entry(&mut self, index: &Value) -> Result<()> {
        self.json()?.begin_entry(index)
    }
    fn end_entry(&mut self) -> Result<()> {
        self.json()?.end_entry()
    }
    fn begin_key(&mut self) -> Result<()> {
        self.json()?.begin_key()
    }
    fn end_key(&mut self) -> Result<()> {
        self.json()?.end_key()
    }
    fn begin_value(&mut self) -> Result<()> {
        self.json()?.begin_value()
    }
    fn end_value(&mut self) -> Result<()> {
        self.json()?.end_value()
    }
    fn on_flags(&mut self, declared: &[String]) -> Result<ValueSpec> {
        self.json()?.on_flags(declared)
    }
    fn on_bool(&mut self) -> Result<ValueSpec> {
        self.json()?.on_bool()
    }
    fn on_char(&mut self) -> Result<ValueSpec> {
        self.json()?.on_char()
    }
    fn on_s8(&mut self) -> Result<ValueSpec> {
        self.json()?.on_s8()
    }
    fn on_s16(&mut self) -> Result<ValueSpec> {
        self.json()?.on_s16()
    }
    fn on_s32(&mut self) -> Result<ValueSpec> {
        self.json()?.on_s32()
    }
    fn on_s64(&mut self) -> Result<ValueSpec> {
        self.json()?.on_s64()
    }
    fn on_u8(&mut self) -> Result<ValueSpec> {
        self.json()?.on_u8()
    }
    fn on_u32(&mut self) -> Result<ValueSpec> {
        self.json()?.on_u32()
    }
    fn on_u64(&mut self) -> Result<ValueSpec> {
        self.json()?.on_u64()
    }
    fn on_f32(&mut self) -> Result<ValueSpec> {
        self.json()?.on_f32()
    }
    fn on_f64(&mut self) -> Result<ValueSpec> {
        self.json()?.on_f64()
    }
}
