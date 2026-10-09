//! Random WIT types, their WIT text, and random values of them.
//!
//! Generation is weighted toward what the canonical ABI lays out differently
//! in memory and in flat form: narrow fields (`bool`, `u8`, `u16`, enum and
//! variant discriminants, flags), variant payloads aligned past their
//! discriminant, joins of differently typed payloads, and values wider than
//! the flat limits.

use wasmtime::component::Val;

/// A small deterministic generator, so a failing seed reproduces.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    pub fn next(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value in `0..n`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }

    /// True with probability `percent`/100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

/// A WIT type. Named kinds carry the name they are defined under.
#[derive(Clone, Debug)]
pub enum Ty {
    Bool,
    U8,
    S8,
    U16,
    S16,
    U32,
    S32,
    U64,
    S64,
    F32,
    F64,
    Char,
    String,
    List(Box<Ty>),
    FixedList(Box<Ty>, u32),
    Map(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    Result(Option<Box<Ty>>, Option<Box<Ty>>),
    Tuple(Vec<Ty>),
    Record(String, Vec<(String, Ty)>),
    Variant(String, Vec<(String, Option<Ty>)>),
    Enum(String, Vec<String>),
    Flags(String, Vec<String>),
}

impl Ty {
    /// The type as it appears in a WIT type expression.
    pub fn wit(&self) -> String {
        match self {
            Ty::Bool => "bool".into(),
            Ty::U8 => "u8".into(),
            Ty::S8 => "s8".into(),
            Ty::U16 => "u16".into(),
            Ty::S16 => "s16".into(),
            Ty::U32 => "u32".into(),
            Ty::S32 => "s32".into(),
            Ty::U64 => "u64".into(),
            Ty::S64 => "s64".into(),
            Ty::F32 => "f32".into(),
            Ty::F64 => "f64".into(),
            Ty::Char => "char".into(),
            Ty::String => "string".into(),
            Ty::List(t) => format!("list<{}>", t.wit()),
            Ty::FixedList(t, n) => format!("list<{}, {n}>", t.wit()),
            Ty::Map(k, v) => format!("map<{}, {}>", k.wit(), v.wit()),
            Ty::Option(t) => format!("option<{}>", t.wit()),
            Ty::Result(None, None) => "result".into(),
            Ty::Result(Some(ok), None) => format!("result<{}>", ok.wit()),
            Ty::Result(None, Some(err)) => format!("result<_, {}>", err.wit()),
            Ty::Result(Some(ok), Some(err)) => format!("result<{}, {}>", ok.wit(), err.wit()),
            Ty::Tuple(ts) => format!(
                "tuple<{}>",
                ts.iter().map(Ty::wit).collect::<Vec<_>>().join(", ")
            ),
            Ty::Record(name, _) | Ty::Variant(name, _) | Ty::Enum(name, _) | Ty::Flags(name, _) => {
                name.clone()
            }
        }
    }

    /// The definitions of every named type this one uses, innermost first.
    pub fn definitions(&self, out: &mut Vec<String>) {
        match self {
            Ty::List(t) | Ty::FixedList(t, _) | Ty::Option(t) => t.definitions(out),
            Ty::Map(k, v) => {
                k.definitions(out);
                v.definitions(out);
            }
            Ty::Result(ok, err) => {
                for t in [ok, err].into_iter().flatten() {
                    t.definitions(out);
                }
            }
            Ty::Tuple(ts) => ts.iter().for_each(|t| t.definitions(out)),
            Ty::Record(name, fields) => {
                fields.iter().for_each(|(_, t)| t.definitions(out));
                let fields: Vec<String> = fields
                    .iter()
                    .map(|(n, t)| format!("{n}: {}", t.wit()))
                    .collect();
                out.push(format!("record {name} {{ {} }}", fields.join(", ")));
            }
            Ty::Variant(name, cases) => {
                cases
                    .iter()
                    .flat_map(|(_, t)| t)
                    .for_each(|t| t.definitions(out));
                let cases: Vec<String> = cases
                    .iter()
                    .map(|(n, t)| match t {
                        Some(t) => format!("{n}({})", t.wit()),
                        None => n.clone(),
                    })
                    .collect();
                out.push(format!("variant {name} {{ {} }}", cases.join(", ")));
            }
            Ty::Enum(name, cases) => out.push(format!("enum {name} {{ {} }}", cases.join(", "))),
            Ty::Flags(name, flags) => out.push(format!("flags {name} {{ {} }}", flags.join(", "))),
            _ => {}
        }
    }

    /// Whether a value of this type can be a map key.
    fn is_key(&self) -> bool {
        matches!(
            self,
            Ty::Bool
                | Ty::U8
                | Ty::S8
                | Ty::U16
                | Ty::S16
                | Ty::U32
                | Ty::S32
                | Ty::U64
                | Ty::S64
                | Ty::Char
                | Ty::String
        )
    }

    /// How many core values this type flattens to, uncapped, by the
    /// canonical ABI's `flatten_type`: a variant is its discriminant plus
    /// its widest case.
    pub fn flats(&self) -> usize {
        match self {
            Ty::String | Ty::List(_) | Ty::Map(..) => 2,
            Ty::FixedList(t, n) => t.flats() * *n as usize,
            Ty::Option(t) => 1 + t.flats(),
            Ty::Result(ok, err) => {
                1 + [ok, err]
                    .into_iter()
                    .flatten()
                    .map(|t| t.flats())
                    .max()
                    .unwrap_or(0)
            }
            Ty::Tuple(ts) => ts.iter().map(Ty::flats).sum(),
            Ty::Record(_, fields) => fields.iter().map(|(_, t)| t.flats()).sum(),
            Ty::Variant(_, cases) => {
                1 + cases
                    .iter()
                    .flat_map(|(_, t)| t)
                    .map(Ty::flats)
                    .max()
                    .unwrap_or(0)
            }
            _ => 1,
        }
    }

    /// The types this one is built from, one level down, or `None` for a leaf.
    /// A composite may have none, such as a variant whose cases have no
    /// payload.
    fn members(&self) -> Option<Vec<&Ty>> {
        Some(match self {
            Ty::List(t) | Ty::FixedList(t, _) | Ty::Option(t) => vec![t],
            Ty::Map(k, v) => vec![k, v],
            Ty::Result(ok, err) => [ok, err].into_iter().flatten().map(|t| &**t).collect(),
            Ty::Tuple(ts) => ts.iter().collect(),
            Ty::Record(_, fields) => fields.iter().map(|(_, t)| t).collect(),
            Ty::Variant(_, cases) => cases.iter().flat_map(|(_, t)| t).collect(),
            _ => return None,
        })
    }

    /// How deeply composites nest, a leaf being 0.
    pub fn depth(&self) -> usize {
        match self.members() {
            None => 0,
            Some(members) => 1 + members.into_iter().map(Ty::depth).max().unwrap_or(0),
        }
    }

    /// The most cases of any enum or variant within this type, which decides
    /// its discriminant's width.
    pub fn most_cases(&self) -> usize {
        let own = match self {
            Ty::Enum(_, cases) => cases.len(),
            Ty::Variant(_, cases) => cases.len(),
            _ => 0,
        };
        let in_members = self
            .members()
            .unwrap_or_default()
            .into_iter()
            .map(Ty::most_cases)
            .max()
            .unwrap_or(0);
        own.max(in_members)
    }

    /// The most flags of any flags type within this type.
    pub fn most_flags(&self) -> usize {
        match self {
            Ty::Flags(_, flags) => flags.len(),
            _ => self
                .members()
                .unwrap_or_default()
                .into_iter()
                .map(Ty::most_flags)
                .max()
                .unwrap_or(0),
        }
    }

    /// The most values one value of this type holds, not counting the
    /// contents of lists, strings, and maps, whose lengths are chosen when
    /// generating. Bounds what a list element costs.
    fn fixed_nodes(&self) -> usize {
        match self {
            Ty::FixedList(t, n) => 1 + t.fixed_nodes() * *n as usize,
            Ty::Option(t) => 1 + t.fixed_nodes(),
            Ty::Result(ok, err) => {
                1 + [ok, err]
                    .into_iter()
                    .flatten()
                    .map(|t| t.fixed_nodes())
                    .max()
                    .unwrap_or(0)
            }
            Ty::Tuple(ts) => 1 + ts.iter().map(Ty::fixed_nodes).sum::<usize>(),
            Ty::Record(_, fields) => 1 + fields.iter().map(|(_, t)| t.fixed_nodes()).sum::<usize>(),
            Ty::Variant(_, cases) => {
                1 + cases
                    .iter()
                    .flat_map(|(_, t)| t)
                    .map(Ty::fixed_nodes)
                    .max()
                    .unwrap_or(0)
            }
            _ => 1,
        }
    }
}

/// Generates types, naming each named type uniquely.
pub struct TypeGen<'r> {
    rng: &'r mut Rng,
    names: usize,
}

impl<'r> TypeGen<'r> {
    pub fn new(rng: &'r mut Rng) -> Self {
        TypeGen { rng, names: 0 }
    }

    fn name(&mut self, prefix: &str) -> String {
        self.names += 1;
        format!("{prefix}{}", self.names)
    }

    pub fn ty(&mut self, depth: u32) -> Ty {
        // Leaf types only once deep enough. Mostly composites otherwise, since
        // they can uncover obscure bugs in how members are laid out.
        if depth == 0 || self.rng.chance(35) {
            return self.leaf();
        }
        match self.rng.below(12) {
            0 => Ty::List(Box::new(self.ty(depth - 1))),
            1 => Ty::Option(Box::new(self.ty(depth - 1))),
            2 => {
                let ok = self.rng.chance(75).then(|| Box::new(self.ty(depth - 1)));
                let err = self.rng.chance(75).then(|| Box::new(self.ty(depth - 1)));
                Ty::Result(ok, err)
            }
            3 => {
                let n = 1 + self.rng.below(4) as usize;
                Ty::Tuple((0..n).map(|_| self.ty(depth - 1)).collect())
            }
            4..=6 => self.record(depth),
            7 | 8 => self.variant(depth),
            9 => self.wide_record(),
            10 => {
                let n = 1 + self.rng.below(5) as u32;
                Ty::FixedList(Box::new(self.ty(depth - 1)), n)
            }
            _ => {
                let key = loop {
                    let key = self.leaf();
                    if key.is_key() {
                        break key;
                    }
                };
                Ty::Map(Box::new(key), Box::new(self.ty(depth - 1)))
            }
        }
    }

    pub fn leaf(&mut self) -> Ty {
        match self.rng.below(20) {
            // Narrow leaves are where memory and flat layouts differ most.
            0..=2 => Ty::Bool,
            3 | 4 => Ty::U8,
            5 => Ty::S8,
            6 => Ty::U16,
            7 => Ty::S16,
            8 => Ty::U32,
            9 => Ty::S32,
            10 => Ty::U64,
            11 => Ty::S64,
            12 => Ty::F32,
            13 => Ty::F64,
            14 => Ty::Char,
            15 | 16 => Ty::String,
            17 => self.enumeration(),
            _ => self.flags(),
        }
    }

    fn record(&mut self, depth: u32) -> Ty {
        let n = 1 + self.rng.below(5) as usize;
        let fields = (0..n)
            .map(|i| (format!("m{i}"), self.ty(depth - 1)))
            .collect();
        Ty::Record(self.name("r"), fields)
    }

    /// A record of narrow leaves flattening past `MAX_FLAT_PARAMS`.
    fn wide_record(&mut self) -> Ty {
        let n = 17 + self.rng.below(8) as usize;
        let fields = (0..n)
            .map(|i| {
                let ty = match self.rng.below(4) {
                    0 => Ty::Bool,
                    1 => Ty::U8,
                    2 => Ty::U16,
                    _ => self.leaf(),
                };
                (format!("m{i}"), ty)
            })
            .collect();
        Ty::Record(self.name("w"), fields)
    }

    fn variant(&mut self, depth: u32) -> Ty {
        let n = 1 + self.rng.below(5) as usize;
        let cases = (0..n)
            .map(|i| {
                let payload = self.rng.chance(70).then(|| self.ty(depth - 1));
                (format!("c{i}"), payload)
            })
            .collect();
        Ty::Variant(self.name("v"), cases)
    }

    fn enumeration(&mut self) -> Ty {
        // Past 256 cases the discriminant is a u16.
        let n = if self.rng.chance(15) {
            257 + self.rng.below(40) as usize
        } else {
            1 + self.rng.below(6) as usize
        };
        Ty::Enum(self.name("e"), (0..n).map(|i| format!("e{i}")).collect())
    }

    fn flags(&mut self) -> Ty {
        // The widths change at 8 and 16 flags.
        let n = *self.rng.pick(&[1, 3, 8, 9, 15, 16, 17, 24, 32]);
        Ty::Flags(self.name("g"), (0..n).map(|i| format!("g{i}")).collect())
    }

    /// A type past one of the limits, so that a random run crosses each of
    /// them rather than only by chance: past `MAX_FLAT_PARAMS` (16 flats),
    /// past what can be flattened at all (64), a discriminant wider than a
    /// `u8`, or deep nesting.
    pub fn boundary(&mut self) -> Ty {
        match self.rng.below(4) {
            0 => self.wide_record(),
            1 => self.very_wide(),
            2 => self.wide_variant(),
            _ => {
                let depth = 8 + self.rng.below(5) as u32;
                self.deep(depth)
            }
        }
    }

    /// Past 64 flats: a record of narrow leaves, or wide records nested in a
    /// fixed-length list.
    fn very_wide(&mut self) -> Ty {
        if self.rng.chance(50) {
            let n = 65 + self.rng.below(16) as usize;
            let fields = (0..n).map(|i| (format!("m{i}"), self.leaf())).collect();
            Ty::Record(self.name("w"), fields)
        } else {
            let member = Ty::Tuple(vec![self.wide_record(), self.leaf()]);
            Ty::FixedList(Box::new(member), 4 + self.rng.below(3) as u32)
        }
    }

    /// A variant with a `u16` discriminant, some of its cases carrying a
    /// payload.
    fn wide_variant(&mut self) -> Ty {
        let n = 257 + self.rng.below(40) as usize;
        let cases = (0..n)
            .map(|i| {
                let payload = self.rng.chance(30).then(|| self.leaf());
                (format!("c{i}"), payload)
            })
            .collect();
        Ty::Variant(self.name("v"), cases)
    }

    /// Composites nested `depth` deep, one kind per level.
    fn deep(&mut self, depth: u32) -> Ty {
        if depth == 0 {
            return self.leaf();
        }
        let inner = self.deep(depth - 1);
        match self.rng.below(6) {
            0 => Ty::Option(Box::new(inner)),
            1 => Ty::Record(
                self.name("d"),
                vec![("f0".into(), inner), ("f1".into(), self.leaf())],
            ),
            2 => Ty::List(Box::new(inner)),
            3 => Ty::Variant(
                self.name("d"),
                vec![("c0".into(), Some(inner)), ("c1".into(), None)],
            ),
            4 => Ty::Result(Some(Box::new(inner)), Some(Box::new(self.leaf()))),
            _ => Ty::Tuple(vec![inner, self.leaf()]),
        }
    }
}

/// The most values (list elements, string characters, and the members of
/// each) that the variable-length parts of one generated value hold, across
/// all of its nesting. A host receiving a value pays for each one (see the
/// hostcall fuel in the harness), so this bounds what one call can cost.
pub const MAX_ELEMENTS: usize = 300_000;

/// Generates values, with sizes ranging from empty to large.
pub struct ValGen<'r> {
    rng: &'r mut Rng,
    /// What remains of [`MAX_ELEMENTS`] for the value being generated.
    budget: usize,
}

impl<'r> ValGen<'r> {
    pub fn new(rng: &'r mut Rng) -> Self {
        ValGen {
            rng,
            budget: MAX_ELEMENTS,
        }
    }

    /// A length: usually small, sometimes empty, occasionally large, and
    /// never more than the budget left allows when each item costs `cost`
    /// values, which is then taken from the budget.
    fn length(&mut self, large: usize, cost: usize) -> usize {
        let wanted = match self.rng.below(20) {
            0 | 1 => 0,
            2 => large / 2 + self.rng.below(large as u64 / 2 + 1) as usize,
            _ => 1 + self.rng.below(8) as usize,
        };
        let cost = cost.max(1);
        let length = wanted.min(self.budget / cost);
        self.budget -= length * cost;
        length
    }

    pub fn val(&mut self, ty: &Ty) -> Val {
        match ty {
            Ty::Bool => Val::Bool(self.rng.chance(50)),
            Ty::U8 => Val::U8(self.rng.next() as u8),
            Ty::S8 => Val::S8(self.rng.next() as i8),
            Ty::U16 => Val::U16(self.rng.next() as u16),
            Ty::S16 => Val::S16(self.rng.next() as i16),
            Ty::U32 => Val::U32(self.rng.next() as u32),
            Ty::S32 => Val::S32(self.rng.next() as i32),
            Ty::U64 => Val::U64(self.rng.next()),
            Ty::S64 => Val::S64(self.rng.next() as i64),
            Ty::F32 => {
                let value = self.float() as f32;
                Val::Float32(if value.is_finite() { value } else { 1.0 })
            }
            Ty::F64 => Val::Float64(self.float()),
            Ty::Char => Val::Char(self.char()),
            Ty::String => {
                let n = self.length(70_000, 1);
                Val::String((0..n).map(|_| self.char()).collect())
            }
            Ty::List(t) => {
                let n = self.length(200_000, t.fixed_nodes());
                Val::List((0..n).map(|_| self.val(t)).collect())
            }
            Ty::FixedList(t, n) => Val::FixedLengthList((0..*n).map(|_| self.val(t)).collect()),
            Ty::Map(k, v) => {
                let n = self.length(100, 1 + k.fixed_nodes() + v.fixed_nodes());
                let mut entries: Vec<(Val, Val)> = Vec::new();
                for _ in 0..n {
                    let key = self.val(k);
                    // Unique keys, so the round trip is exact.
                    if entries.iter().all(|(existing, _)| existing != &key) {
                        entries.push((key, self.val(v)));
                    }
                }
                Val::Map(entries)
            }
            Ty::Option(t) => Val::Option(self.rng.chance(70).then(|| Box::new(self.val(t)))),
            Ty::Result(ok, err) => {
                if self.rng.chance(50) {
                    Val::Result(Ok(ok.as_ref().map(|t| Box::new(self.val(t)))))
                } else {
                    Val::Result(Err(err.as_ref().map(|t| Box::new(self.val(t)))))
                }
            }
            Ty::Tuple(ts) => Val::Tuple(ts.iter().map(|t| self.val(t)).collect()),
            Ty::Record(_, fields) => Val::Record(
                fields
                    .iter()
                    .map(|(name, t)| (name.clone(), self.val(t)))
                    .collect(),
            ),
            Ty::Variant(_, cases) => {
                let (name, payload) = self.rng.pick(cases).clone();
                Val::Variant(name, payload.map(|t| Box::new(self.val(&t))))
            }
            Ty::Enum(_, cases) => Val::Enum(self.rng.pick(cases).clone()),
            Ty::Flags(_, flags) => Val::Flags(
                flags
                    .iter()
                    .filter(|_| self.rng.chance(50))
                    .cloned()
                    .collect(),
            ),
        }
    }

    /// A finite float, including signs, zero, and extreme magnitudes.
    fn float(&mut self) -> f64 {
        match self.rng.below(6) {
            0 => 0.0,
            1 => -1.5,
            2 => f64::from(f32::MAX),
            _ => {
                let bits = self.rng.next();
                let value = f64::from_bits(bits);
                if value.is_finite() { value } else { 1.0 }
            }
        }
    }

    fn char(&mut self) -> char {
        loop {
            let code = match self.rng.below(4) {
                0 => self.rng.below(0x80) as u32,
                1 => self.rng.below(0x800) as u32,
                2 => self.rng.below(0x1_0000) as u32,
                _ => self.rng.below(0x11_0000) as u32,
            };
            if let Some(c) = char::from_u32(code) {
                return c;
            }
        }
    }
}
