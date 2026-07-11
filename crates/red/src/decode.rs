//! Hand-rolled decoders for binary value formats the inspector's lens can show
//! (see docs/plans/redis.md's "binary value decoders" gap): MessagePack,
//! schemaless Protocol Buffers, and Python pickle. Each takes raw bytes and
//! returns a human-readable rendering, or `None` when the bytes aren't that
//! format (the caller then falls back to raw/hex).
//!
//! Pure and dependency-free on purpose: the project ships no serialization
//! crates (its JSON pretty-printer is hand-rolled too), so these are small byte
//! walkers rather than a pile of new dependencies. They're *viewers*, not
//! round-trip codecs — the output is a readable JSON-ish tree, not necessarily
//! valid JSON (map keys can be non-string, bytes render as hex), and a format
//! this build doesn't fully understand degrades to `None` rather than guessing.

/// A decoded value tree, shared by the MessagePack and pickle decoders (both
/// map naturally onto this shape). Rendered by [`Decoded::render`].
#[derive(Debug, Clone, PartialEq)]
enum Decoded {
    Null,
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Array(Vec<Decoded>),
    Map(Vec<(Decoded, Decoded)>),
    /// An opaque reference the viewer can't expand (a pickle global/reduce, a
    /// msgpack extension type): shown verbatim.
    Other(String),
}

/// Guard against a pathological deeply-nested input blowing the stack.
const MAX_DEPTH: usize = 64;

impl Decoded {
    /// Render as a readable, indented, JSON-ish tree (2-space indent, matching
    /// the inspector's `pretty_json`).
    fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        match self {
            Decoded::Null => out.push_str("null"),
            Decoded::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Decoded::Int(n) => out.push_str(&n.to_string()),
            Decoded::UInt(n) => out.push_str(&n.to_string()),
            Decoded::Float(x) => out.push_str(&x.to_string()),
            Decoded::Str(s) => {
                out.push('"');
                escape_into(s, out);
                out.push('"');
            }
            Decoded::Bytes(b) => out.push_str(&render_bytes(b)),
            Decoded::Other(s) => out.push_str(s),
            Decoded::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (i, it) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline_indent(out, depth + 1);
                    it.write(out, depth + 1);
                }
                newline_indent(out, depth);
                out.push(']');
            }
            Decoded::Map(pairs) => {
                if pairs.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline_indent(out, depth + 1);
                    k.write(out, depth + 1);
                    out.push_str(": ");
                    v.write(out, depth + 1);
                }
                newline_indent(out, depth);
                out.push('}');
            }
        }
    }
}

fn newline_indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Minimal JSON string escaping for the readable rendering.
fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
}

/// Render a byte string compactly: short ones as a `0x…` hex literal, long ones
/// with a length note so the tree stays readable.
fn render_bytes(b: &[u8]) -> String {
    const MAX: usize = 32;
    let shown = b.len().min(MAX);
    let hex: String = b[..shown].iter().map(|x| format!("{x:02x}")).collect();
    if b.len() > shown {
        format!("0x{hex}… ({} bytes)", b.len())
    } else {
        format!("0x{hex}")
    }
}

// --- a shared little-endian / big-endian cursor ---------------------------

struct Cur<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Cur { b, i: 0 }
    }
    fn done(&self) -> bool {
        self.i >= self.b.len()
    }
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.i)?;
        self.i += 1;
        Some(v)
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(s)
    }
}

// --- MessagePack ----------------------------------------------------------

/// Decode a MessagePack value, requiring the whole input to be one consumed
/// value (a clean "is this msgpack" signal). `None` on any malformed byte.
pub fn decode_msgpack(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let mut cur = Cur::new(bytes);
    let v = msgpack_value(&mut cur, 0)?;
    cur.done().then(|| v.render())
}

fn msgpack_value(cur: &mut Cur, depth: usize) -> Option<Decoded> {
    if depth > MAX_DEPTH {
        return None;
    }
    let tag = cur.u8()?;
    Some(match tag {
        0x00..=0x7f => Decoded::UInt(tag as u64), // positive fixint
        0xe0..=0xff => Decoded::Int((tag as i8) as i64), // negative fixint
        0x80..=0x8f => msgpack_map(cur, (tag & 0x0f) as usize, depth)?,
        0x90..=0x9f => msgpack_array(cur, (tag & 0x0f) as usize, depth)?,
        0xa0..=0xbf => msgpack_str(cur, (tag & 0x1f) as usize)?,
        0xc0 => Decoded::Null,
        0xc2 => Decoded::Bool(false),
        0xc3 => Decoded::Bool(true),
        0xcc => Decoded::UInt(cur.u8()? as u64),
        0xcd => Decoded::UInt(be_uint(cur, 2)?),
        0xce => Decoded::UInt(be_uint(cur, 4)?),
        0xcf => Decoded::UInt(be_uint(cur, 8)?),
        0xd0 => Decoded::Int((cur.u8()? as i8) as i64),
        0xd1 => Decoded::Int(be_int(cur, 2)?),
        0xd2 => Decoded::Int(be_int(cur, 4)?),
        0xd3 => Decoded::Int(be_int(cur, 8)?),
        0xca => Decoded::Float(f32::from_be_bytes(cur.take(4)?.try_into().ok()?) as f64),
        0xcb => Decoded::Float(f64::from_be_bytes(cur.take(8)?.try_into().ok()?)),
        0xd9 => {
            let n = cur.u8()? as usize;
            msgpack_str(cur, n)?
        }
        0xda => {
            let n = be_uint(cur, 2)? as usize;
            msgpack_str(cur, n)?
        }
        0xdb => {
            let n = be_uint(cur, 4)? as usize;
            msgpack_str(cur, n)?
        }
        0xc4 => {
            let n = cur.u8()? as usize;
            Decoded::Bytes(cur.take(n)?.to_vec())
        }
        0xc5 => {
            let n = be_uint(cur, 2)? as usize;
            Decoded::Bytes(cur.take(n)?.to_vec())
        }
        0xc6 => {
            let n = be_uint(cur, 4)? as usize;
            Decoded::Bytes(cur.take(n)?.to_vec())
        }
        0xdc => {
            let n = be_uint(cur, 2)? as usize;
            msgpack_array(cur, n, depth)?
        }
        0xdd => {
            let n = be_uint(cur, 4)? as usize;
            msgpack_array(cur, n, depth)?
        }
        0xde => {
            let n = be_uint(cur, 2)? as usize;
            msgpack_map(cur, n, depth)?
        }
        0xdf => {
            let n = be_uint(cur, 4)? as usize;
            msgpack_map(cur, n, depth)?
        }
        // fixext 1/2/4/8/16: a type byte plus that many data bytes.
        0xd4 => msgpack_ext(cur, 1)?,
        0xd5 => msgpack_ext(cur, 2)?,
        0xd6 => msgpack_ext(cur, 4)?,
        0xd7 => msgpack_ext(cur, 8)?,
        0xd8 => msgpack_ext(cur, 16)?,
        0xc7 => {
            let n = cur.u8()? as usize;
            msgpack_ext(cur, n)?
        }
        0xc8 => {
            let n = be_uint(cur, 2)? as usize;
            msgpack_ext(cur, n)?
        }
        0xc9 => {
            let n = be_uint(cur, 4)? as usize;
            msgpack_ext(cur, n)?
        }
        0xc1 => return None, // never-used byte
    })
}

fn be_uint(cur: &mut Cur, n: usize) -> Option<u64> {
    let mut v = 0u64;
    for &b in cur.take(n)? {
        v = (v << 8) | b as u64;
    }
    Some(v)
}

fn be_int(cur: &mut Cur, n: usize) -> Option<i64> {
    let raw = be_uint(cur, n)?;
    // Sign-extend from an n-byte two's-complement value.
    let bits = n * 8;
    Some(((raw << (64 - bits)) as i64) >> (64 - bits))
}

fn msgpack_str(cur: &mut Cur, n: usize) -> Option<Decoded> {
    Some(Decoded::Str(
        String::from_utf8_lossy(cur.take(n)?).into_owned(),
    ))
}

fn msgpack_array(cur: &mut Cur, n: usize, depth: usize) -> Option<Decoded> {
    let mut items = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        items.push(msgpack_value(cur, depth + 1)?);
    }
    Some(Decoded::Array(items))
}

fn msgpack_map(cur: &mut Cur, n: usize, depth: usize) -> Option<Decoded> {
    let mut pairs = Vec::with_capacity(n.min(1024));
    for _ in 0..n {
        let k = msgpack_value(cur, depth + 1)?;
        let v = msgpack_value(cur, depth + 1)?;
        pairs.push((k, v));
    }
    Some(Decoded::Map(pairs))
}

fn msgpack_ext(cur: &mut Cur, len: usize) -> Option<Decoded> {
    let ty = cur.u8()? as i8;
    let data = cur.take(len)?;
    Some(Decoded::Other(format!(
        "ext(type {ty}, {})",
        render_bytes(data)
    )))
}

// --- Protocol Buffers (schemaless) ----------------------------------------

/// Decode a length-delimited protobuf message without a schema: a tree of
/// `field <n> (<wire type>): <value>`, recursing into nested messages. Requires
/// the whole input to parse and have at least one field (the "is this protobuf"
/// signal, since almost any short byte string parses as *some* varints).
pub fn decode_protobuf(bytes: &[u8]) -> Option<String> {
    let fields = protobuf_message(bytes)?;
    if fields.is_empty() {
        return None;
    }
    let mut out = String::new();
    render_protobuf(&fields, &mut out, 0);
    Some(out)
}

enum PbField {
    Varint(u64, u64),
    Fixed64(u64, u64),
    Fixed32(u64, u32),
    Len(u64, Vec<u8>),
}

/// Parse a whole message; `None` if the bytes don't cleanly consume as a
/// sequence of fields.
fn protobuf_message(bytes: &[u8]) -> Option<Vec<PbField>> {
    let mut cur = Cur::new(bytes);
    let mut fields = Vec::new();
    while !cur.done() {
        let tag = varint(&mut cur)?;
        let field = tag >> 3;
        let wire = tag & 7;
        if field == 0 {
            return None; // field numbers start at 1
        }
        let f = match wire {
            0 => PbField::Varint(field, varint(&mut cur)?),
            1 => PbField::Fixed64(field, be_le_u64(&mut cur)?),
            5 => PbField::Fixed32(field, be_le_u32(&mut cur)?),
            2 => {
                let len = varint(&mut cur)? as usize;
                PbField::Len(field, cur.take(len)?.to_vec())
            }
            _ => return None, // groups (3/4) and anything else: not supported
        };
        fields.push(f);
    }
    Some(fields)
}

fn varint(cur: &mut Cur) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..10).map(|i| i * 7) {
        let b = cur.u8()?;
        v |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None // more than 10 bytes: not a valid varint
}

fn be_le_u64(cur: &mut Cur) -> Option<u64> {
    Some(u64::from_le_bytes(cur.take(8)?.try_into().ok()?))
}

fn be_le_u32(cur: &mut Cur) -> Option<u32> {
    Some(u32::from_le_bytes(cur.take(4)?.try_into().ok()?))
}

fn render_protobuf(fields: &[PbField], out: &mut String, depth: usize) {
    for f in fields {
        for _ in 0..depth {
            out.push_str("  ");
        }
        match f {
            PbField::Varint(n, v) => out.push_str(&format!("field {n} (varint): {v}\n")),
            PbField::Fixed64(n, v) => out.push_str(&format!(
                "field {n} (fixed64): {v} (f64 {})\n",
                f64::from_bits(*v)
            )),
            PbField::Fixed32(n, v) => out.push_str(&format!(
                "field {n} (fixed32): {v} (f32 {})\n",
                f32::from_bits(*v)
            )),
            PbField::Len(n, data) => {
                // A length-delimited field is a nested message, a string, or raw
                // bytes. Prefer a nested message when it parses cleanly; else a
                // printable string; else hex.
                if let Some(nested) = protobuf_message(data).filter(|m| !m.is_empty()) {
                    out.push_str(&format!("field {n} (message):\n"));
                    render_protobuf(&nested, out, depth + 1);
                } else if let Ok(s) = std::str::from_utf8(data) {
                    if s.chars().all(|c| !c.is_control() || c == '\n' || c == '\t') {
                        out.push_str(&format!("field {n} (string): {s:?}\n"));
                    } else {
                        out.push_str(&format!("field {n} (bytes): {}\n", render_bytes(data)));
                    }
                } else {
                    out.push_str(&format!("field {n} (bytes): {}\n", render_bytes(data)));
                }
            }
        }
    }
}

// --- Python pickle (common opcodes) ---------------------------------------

/// Decode a Python pickle, covering the opcodes real-world pickles use
/// (protocols 0–5 core: ints/floats/strings/bytes, lists/tuples/dicts/sets,
/// memo, and globals/reduce shown opaquely). Unknown opcodes stop the decode
/// (returning `None`) so the caller falls back to hex rather than showing a
/// half-decoded guess.
pub fn decode_pickle(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let mut cur = Cur::new(bytes);
    let mut stack: Vec<Decoded> = Vec::new();
    let mut marks: Vec<usize> = Vec::new();
    let mut memo: std::collections::HashMap<u64, Decoded> = std::collections::HashMap::new();

    loop {
        let op = cur.u8()?;
        match op {
            0x80 => {
                cur.u8()?; // PROTO <n>
            }
            0x95 => {
                cur.take(8)?; // FRAME <len8>
            }
            b'.' => break, // STOP
            b'(' => marks.push(stack.len()),
            b'0' => {
                stack.pop()?; // POP
            }
            b'2' => {
                let top = stack.last()?.clone(); // DUP
                stack.push(top);
            }
            b'N' => stack.push(Decoded::Null),
            0x88 => stack.push(Decoded::Bool(true)),
            0x89 => stack.push(Decoded::Bool(false)),
            b'K' => stack.push(Decoded::UInt(cur.u8()? as u64)), // BININT1
            b'M' => stack.push(Decoded::UInt(le_uint(&mut cur, 2)?)), // BININT2
            b'J' => stack.push(Decoded::Int(le_int(&mut cur, 4)?)), // BININT
            0x8a => stack.push(Decoded::Int(pickle_long(&mut cur)?)), // LONG1
            b'G' => stack.push(Decoded::Float(f64::from_be_bytes(
                cur.take(8)?.try_into().ok()?,
            ))),
            0x8c => stack.push(pickle_str(&mut cur, 1)?), // SHORT_BINUNICODE
            b'X' => stack.push(pickle_str(&mut cur, 4)?), // BINUNICODE
            0x8d => stack.push(pickle_str(&mut cur, 8)?), // BINUNICODE8
            b'U' => stack.push(pickle_str(&mut cur, 1)?), // SHORT_BINSTRING
            b'T' => stack.push(pickle_str(&mut cur, 4)?), // BINSTRING
            b'C' => stack.push(pickle_bytes(&mut cur, 1)?), // SHORT_BINBYTES
            b'B' => stack.push(pickle_bytes(&mut cur, 4)?), // BINBYTES
            0x8e => stack.push(pickle_bytes(&mut cur, 8)?), // BINBYTES8
            b']' | b')' | 0x8f => stack.push(Decoded::Array(Vec::new())), // EMPTY_LIST/TUPLE/SET
            b'}' => stack.push(Decoded::Map(Vec::new())), // EMPTY_DICT
            b'a' => {
                // APPEND
                let v = stack.pop()?;
                if let Some(Decoded::Array(items)) = stack.last_mut() {
                    items.push(v);
                } else {
                    return None;
                }
            }
            b'e' | 0x90 => {
                // APPENDS / ADDITEMS
                let items = pop_to_mark(&mut stack, &mut marks)?;
                if let Some(Decoded::Array(a)) = stack.last_mut() {
                    a.extend(items);
                } else {
                    return None;
                }
            }
            b's' => {
                // SETITEM
                let v = stack.pop()?;
                let k = stack.pop()?;
                if let Some(Decoded::Map(m)) = stack.last_mut() {
                    m.push((k, v));
                } else {
                    return None;
                }
            }
            b'u' => {
                // SETITEMS
                let items = pop_to_mark(&mut stack, &mut marks)?;
                let mut it = items.into_iter();
                if let Some(Decoded::Map(m)) = stack.last_mut() {
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        m.push((k, v));
                    }
                } else {
                    return None;
                }
            }
            0x85 => tuple_n(&mut stack, 1)?, // TUPLE1
            0x86 => tuple_n(&mut stack, 2)?, // TUPLE2
            0x87 => tuple_n(&mut stack, 3)?, // TUPLE3
            b't' | b'l' | 0x91 => {
                // TUPLE / LIST / FROZENSET (to mark)
                let items = pop_to_mark(&mut stack, &mut marks)?;
                stack.push(Decoded::Array(items));
            }
            b'd' => {
                // DICT (to mark)
                let items = pop_to_mark(&mut stack, &mut marks)?;
                let mut it = items.into_iter();
                let mut m = Vec::new();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    m.push((k, v));
                }
                stack.push(Decoded::Map(m));
            }
            0x94 => {
                // MEMOIZE
                memo.insert(memo.len() as u64, stack.last()?.clone());
            }
            b'q' => {
                let i = cur.u8()? as u64; // BINPUT
                memo.insert(i, stack.last()?.clone());
            }
            b'r' => {
                let i = le_uint(&mut cur, 4)?; // LONG_BINPUT
                memo.insert(i, stack.last()?.clone());
            }
            b'h' => {
                let i = cur.u8()? as u64; // BINGET
                stack.push(memo.get(&i).cloned().unwrap_or(Decoded::Null));
            }
            b'j' => {
                let i = le_uint(&mut cur, 4)?; // LONG_BINGET
                stack.push(memo.get(&i).cloned().unwrap_or(Decoded::Null));
            }
            0x93 => {
                // STACK_GLOBAL: module, name on stack
                let name = stack.pop()?;
                let module = stack.pop()?;
                stack.push(Decoded::Other(global_name(&module, &name)));
            }
            b'c' => {
                // GLOBAL: two newline-terminated strings
                let module = read_line(&mut cur)?;
                let name = read_line(&mut cur)?;
                stack.push(Decoded::Other(format!("<{module}.{name}>")));
            }
            b'R' | 0x81 => {
                // REDUCE / NEWOBJ: callable + args on stack -> opaque object
                let args = stack.pop()?;
                let callable = stack.pop()?;
                stack.push(Decoded::Other(format!(
                    "{}({})",
                    opaque(&callable),
                    opaque(&args)
                )));
            }
            b'b' => {
                // BUILD: state on stack, applied to the object below it. We can't
                // meaningfully merge, so drop the state and keep the object.
                stack.pop()?;
            }
            _ => return None, // unknown opcode: bail, don't guess
        }
    }
    stack.pop().map(|v| v.render())
}

fn le_uint(cur: &mut Cur, n: usize) -> Option<u64> {
    let mut v = 0u64;
    for (i, &b) in cur.take(n)?.iter().enumerate() {
        v |= (b as u64) << (i * 8);
    }
    Some(v)
}

fn le_int(cur: &mut Cur, n: usize) -> Option<i64> {
    let raw = le_uint(cur, n)?;
    let bits = n * 8;
    Some(((raw << (64 - bits)) as i64) >> (64 - bits))
}

/// A pickle `LONG1`: a 1-byte length then that many little-endian, two's
/// complement bytes. Capped at 8 bytes (bigger arbitrary-precision ints are
/// shown as-is-truncated rather than supported fully).
fn pickle_long(cur: &mut Cur) -> Option<i64> {
    let n = cur.u8()? as usize;
    if n == 0 {
        return Some(0);
    }
    let bytes = cur.take(n)?;
    let take = n.min(8);
    let mut v = 0u64;
    for (i, &b) in bytes.iter().take(take).enumerate() {
        v |= (b as u64) << (i * 8);
    }
    let bits = take * 8;
    Some(((v << (64 - bits)) as i64) >> (64 - bits))
}

fn pickle_str(cur: &mut Cur, len_bytes: usize) -> Option<Decoded> {
    let n = le_uint(cur, len_bytes)? as usize;
    Some(Decoded::Str(
        String::from_utf8_lossy(cur.take(n)?).into_owned(),
    ))
}

fn pickle_bytes(cur: &mut Cur, len_bytes: usize) -> Option<Decoded> {
    let n = le_uint(cur, len_bytes)? as usize;
    Some(Decoded::Bytes(cur.take(n)?.to_vec()))
}

fn read_line(cur: &mut Cur) -> Option<String> {
    let mut s = String::new();
    loop {
        let b = cur.u8()?;
        if b == b'\n' {
            return Some(s);
        }
        s.push(b as char);
    }
}

/// Pop the items pushed since the most recent mark, leaving the container that
/// was below the mark on top of the stack.
fn pop_to_mark(stack: &mut Vec<Decoded>, marks: &mut Vec<usize>) -> Option<Vec<Decoded>> {
    let mark = marks.pop()?;
    if mark > stack.len() {
        return None;
    }
    Some(stack.split_off(mark))
}

fn tuple_n(stack: &mut Vec<Decoded>, n: usize) -> Option<()> {
    if stack.len() < n {
        return None;
    }
    let items = stack.split_off(stack.len() - n);
    stack.push(Decoded::Array(items));
    Some(())
}

fn global_name(module: &Decoded, name: &Decoded) -> String {
    match (module, name) {
        (Decoded::Str(m), Decoded::Str(n)) => format!("<{m}.{n}>"),
        _ => "<global>".to_string(),
    }
}

/// A compact, single-line rendering for embedding a value inside an opaque
/// object/reduce label.
fn opaque(v: &Decoded) -> String {
    match v {
        Decoded::Other(s) => s.clone(),
        Decoded::Str(s) => format!("{s:?}"),
        Decoded::Array(items) => format!("({} items)", items.len()),
        Decoded::Map(m) => format!("({} keys)", m.len()),
        other => other.render(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msgpack_decodes_a_map_with_nested_array() {
        // {"a": 1, "b": [2, 3]}
        let bytes = b"\x82\xa1a\x01\xa1b\x92\x02\x03";
        let out = decode_msgpack(bytes).unwrap();
        assert!(out.contains("\"a\": 1"), "{out}");
        assert!(out.contains("\"b\": ["), "{out}");
        assert!(out.contains("2") && out.contains("3"), "{out}");
    }

    #[test]
    fn msgpack_handles_scalars_and_rejects_trailing_garbage() {
        assert_eq!(decode_msgpack(b"\xc3").unwrap(), "true");
        assert_eq!(decode_msgpack(b"\xcc\x80").unwrap(), "128"); // uint8 128
        assert_eq!(decode_msgpack(b"\xff").unwrap(), "-1"); // negative fixint
                                                            // Trailing byte after a complete value: not a clean single value.
        assert!(decode_msgpack(b"\xc3\x00\x99").is_none());
        assert!(decode_msgpack(b"").is_none());
    }

    #[test]
    fn protobuf_decodes_varint_string_and_nested() {
        // field 1 = varint 150; field 2 = string "testing"
        let bytes = b"\x08\x96\x01\x12\x07testing";
        let out = decode_protobuf(bytes).unwrap();
        assert!(out.contains("field 1 (varint): 150"), "{out}");
        assert!(out.contains("field 2 (string): \"testing\""), "{out}");
    }

    #[test]
    fn protobuf_recurses_into_a_nested_message() {
        // field 3 (len) wraps { field 1 = varint 42 }.
        // inner: 08 2a ; outer: tag=(3<<3|2)=0x1a, len=2
        let bytes = b"\x1a\x02\x08\x2a";
        let out = decode_protobuf(bytes).unwrap();
        assert!(out.contains("field 3 (message):"), "{out}");
        assert!(out.contains("field 1 (varint): 42"), "{out}");
    }

    #[test]
    fn pickle_decodes_a_protocol2_dict() {
        // pickle.dumps({"a": 1}, protocol=2)
        let bytes = b"\x80\x02}q\x00U\x01aq\x01K\x01s.";
        let out = decode_pickle(bytes).unwrap();
        assert!(out.contains("\"a\": 1"), "{out}");
    }

    #[test]
    fn pickle_decodes_a_list_and_scalars() {
        // pickle.dumps([1, True, None], protocol=2):
        // \x80\x02]q\x00(K\x01\x88Ne.
        let bytes = b"\x80\x02]q\x00(K\x01\x88Ne.";
        let out = decode_pickle(bytes).unwrap();
        assert!(out.contains('1'), "{out}");
        assert!(out.contains("true"), "{out}");
        assert!(out.contains("null"), "{out}");
    }

    #[test]
    fn unknown_pickle_opcode_bails() {
        // A lone unsupported opcode should not panic or half-decode.
        assert!(decode_pickle(b"\x80\x02\xfe").is_none());
    }
}
