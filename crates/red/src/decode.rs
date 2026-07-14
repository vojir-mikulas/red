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

/// Guard against a pathological *wide* input exhausting memory. The pickle
/// build loop can clone whole subtrees (DUP, MEMOIZE, BINGET), so a tiny blob
/// of repeated DUP+APPEND doubles the node count every couple of bytes — 2^n
/// growth from ~n bytes. Bounding the total cloned node count turns that DoS
/// into a clean fall-back-to-hex.
const MAX_NODES: usize = 2_000_000;

/// Count the nodes in a value tree, stopping early once `cap` is reached.
/// Iterative (an explicit worklist, not recursion) so counting a deeply nested
/// tree can't itself overflow the stack.
fn node_count_capped(v: &Decoded, cap: usize) -> usize {
    let mut count = 0usize;
    let mut work = vec![v];
    while let Some(cur) = work.pop() {
        count += 1;
        if count > cap {
            return count;
        }
        match cur {
            Decoded::Array(items) => work.extend(items.iter()),
            Decoded::Map(pairs) => {
                for (k, val) in pairs {
                    work.push(k);
                    work.push(val);
                }
            }
            _ => {}
        }
    }
    count
}

impl Decoded {
    /// Render as a readable, indented, JSON-ish tree (2-space indent, matching
    /// the inspector's `pretty_json`).
    fn render(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        // Guard the render recursion the same way the parsers guard theirs: a
        // decoder can build a value tree deeper than MAX_DEPTH (pickle's build
        // loop is iterative and imposes no depth cap), and rendering it would
        // otherwise recurse once per level and blow the stack.
        if depth > MAX_DEPTH {
            out.push('…');
            return;
        }
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
    for i in 0..10 {
        let b = cur.u8()?;
        // The 10th byte only contributes bit 63, so any payload above 0x01
        // there overflows a u64: a real protobuf varint never sets those bits.
        // Reject rather than silently truncating them away.
        if i == 9 && b & 0x7f > 0x01 {
            return None;
        }
        v |= ((b & 0x7f) as u64) << (i * 7);
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
                // Only recurse into a nested message while under the depth cap;
                // a payload of nested length-delimited wrappers would otherwise
                // reparse-and-recurse per level and overflow the stack.
                if let Some(nested) =
                    protobuf_message(data).filter(|m| !m.is_empty() && depth < MAX_DEPTH)
                {
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
    // Running count of nodes produced by cloning ops; bounds the 2^n blow-up.
    let mut total_nodes: usize = 0;

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
                // DUP: clones the top of the stack. Bound the clone so a
                // DUP+APPEND loop can't grow the tree exponentially.
                let top = stack.last()?;
                let n = node_count_capped(top, MAX_NODES - total_nodes);
                if total_nodes + n > MAX_NODES {
                    return None;
                }
                total_nodes += n;
                let dup = top.clone();
                stack.push(dup);
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
                let top = stack.last()?;
                let n = node_count_capped(top, MAX_NODES - total_nodes);
                if total_nodes + n > MAX_NODES {
                    return None;
                }
                total_nodes += n;
                memo.insert(memo.len() as u64, top.clone());
            }
            b'q' => {
                let i = cur.u8()? as u64; // BINPUT
                let top = stack.last()?;
                let n = node_count_capped(top, MAX_NODES - total_nodes);
                if total_nodes + n > MAX_NODES {
                    return None;
                }
                total_nodes += n;
                memo.insert(i, top.clone());
            }
            b'r' => {
                let i = le_uint(&mut cur, 4)?; // LONG_BINPUT
                let top = stack.last()?;
                let n = node_count_capped(top, MAX_NODES - total_nodes);
                if total_nodes + n > MAX_NODES {
                    return None;
                }
                total_nodes += n;
                memo.insert(i, top.clone());
            }
            b'h' => {
                let i = cur.u8()? as u64; // BINGET
                                          // A reference to an undefined memo slot means the pickle is
                                          // malformed/truncated: bail (fall back to hex) rather than
                                          // fabricating a Null.
                let v = memo.get(&i)?;
                let n = node_count_capped(v, MAX_NODES - total_nodes);
                if total_nodes + n > MAX_NODES {
                    return None;
                }
                total_nodes += n;
                let v = v.clone();
                stack.push(v);
            }
            b'j' => {
                let i = le_uint(&mut cur, 4)?; // LONG_BINGET
                let v = memo.get(&i)?;
                let n = node_count_capped(v, MAX_NODES - total_nodes);
                if total_nodes + n > MAX_NODES {
                    return None;
                }
                total_nodes += n;
                let v = v.clone();
                stack.push(v);
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
    // `usize::try_from` (not `as usize`) so an 8-byte length above 2^32 is
    // rejected on a 32-bit target rather than wrapping to a wrong span.
    let n = usize::try_from(le_uint(cur, len_bytes)?).ok()?;
    Some(Decoded::Str(
        String::from_utf8_lossy(cur.take(n)?).into_owned(),
    ))
}

fn pickle_bytes(cur: &mut Cur, len_bytes: usize) -> Option<Decoded> {
    let n = usize::try_from(le_uint(cur, len_bytes)?).ok()?;
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

// ---------------------------------------------------------------------------
// Timestamp lens: a Unix-epoch integer → a human UTC datetime.
// ---------------------------------------------------------------------------

/// Render a Unix-epoch integer as a UTC datetime, auto-detecting the unit
/// (seconds / milliseconds / microseconds / nanoseconds) by magnitude. Accepts
/// the value as ASCII digits (a Redis string like `"1700000000"`) or as a 4- or
/// 8-byte big-endian integer blob. `None` when the bytes aren't a plausible
/// timestamp (below ~1973), so the lens falls back to raw/hex rather than
/// dressing up an ordinary small integer as a date.
pub fn decode_timestamp(bytes: &[u8]) -> Option<String> {
    let raw = parse_epoch_int(bytes)?;
    let (secs, unit) = epoch_to_seconds(raw)?;
    let secs = i64::try_from(secs).ok()?;
    let when = format_unix_utc(secs)?;
    Some(format!("{when}\n(unix {unit}: {raw})"))
}

/// Parse the value as a signed integer, from ASCII digits or a 4-/8-byte
/// big-endian blob. `None` when it isn't a bare integer.
fn parse_epoch_int(bytes: &[u8]) -> Option<i128> {
    if let Ok(s) = std::str::from_utf8(bytes) {
        let t = s.trim();
        let digits_only = !t.is_empty()
            && t.bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_digit() || (i == 0 && b == b'-'));
        if digits_only {
            return t.parse::<i128>().ok();
        }
    }
    match bytes.len() {
        4 => Some(u32::from_be_bytes(bytes.try_into().ok()?) as i128),
        8 => Some(i64::from_be_bytes(bytes.try_into().ok()?) as i128),
        _ => None,
    }
}

/// Detect the epoch unit by magnitude and normalise to seconds. The lower bound
/// (~1e8 s ≈ 1973) rejects small integers that merely happen to be numbers.
fn epoch_to_seconds(v: i128) -> Option<(i128, &'static str)> {
    match v {
        v if v >= 1_000_000_000_000_000_000 => Some((v / 1_000_000_000, "nanos")),
        v if v >= 1_000_000_000_000_000 => Some((v / 1_000_000, "micros")),
        v if v >= 1_000_000_000_000 => Some((v / 1_000, "millis")),
        v if v >= 100_000_000 => Some((v, "seconds")),
        _ => None,
    }
}

/// Format Unix seconds as `YYYY-MM-DD HH:MM:SS UTC`. Pure/allocation-light so
/// the timestamp lens needs no date crate (the project ships none).
fn format_unix_utc(secs: i64) -> Option<String> {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    Some(format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC"))
}

/// Days-since-1970 → `(year, month, day)`, via Howard Hinnant's `civil_from_days`
/// (proleptic Gregorian, exact, branch-light).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Decompression lens: gzip (RFC 1952) / zlib (RFC 1950) over DEFLATE (RFC 1951).
// Hand-rolled inflate (a `puff.c`-style bit-at-a-time Huffman decoder), so a
// compressed cache value shows its payload instead of a wall of hex — with no
// new dependency (a C `zstd`/`flate2` would break the dependency-free rule).
// zstd/LZ4 are deliberately out of scope: they can't be hand-rolled as compactly.
// ---------------------------------------------------------------------------

/// Cap on inflate output, guarding against a decompression bomb (a tiny blob
/// that expands to gigabytes). Far above any value the inspector previews.
const INFLATE_MAX: usize = 16 * 1024 * 1024;

/// Detect and decompress a gzip or zlib stream, returning the inflated bytes.
/// `None` when the input isn't a recognised/valid compressed stream, or would
/// expand past [`INFLATE_MAX`]. Raw headerless DEFLATE is intentionally not
/// probed (no header to detect → too many false positives on arbitrary bytes).
pub fn decompress(bytes: &[u8]) -> Option<Vec<u8>> {
    if let Some(rest) = strip_gzip_header(bytes) {
        return inflate(rest);
    }
    if is_zlib(bytes) {
        // zlib: 2-byte header, then DEFLATE (skip the 4-byte FDICT id if set).
        let start = if bytes[1] & 0x20 != 0 { 6 } else { 2 };
        return inflate(bytes.get(start..)?);
    }
    None
}

/// Strip a gzip header (magic `1f 8b`, CM=deflate, optional FEXTRA/FNAME/
/// FCOMMENT/FHCRC), returning the DEFLATE stream that follows. `None` if it
/// isn't gzip or the header runs off the end.
fn strip_gzip_header(b: &[u8]) -> Option<&[u8]> {
    if b.len() < 10 || b[0] != 0x1f || b[1] != 0x8b || b[2] != 0x08 {
        return None;
    }
    let flg = b[3];
    let mut pos = 10;
    if flg & 0x04 != 0 {
        // FEXTRA: 2-byte length then that many bytes.
        let xlen = *b.get(pos)? as usize | ((*b.get(pos + 1)? as usize) << 8);
        pos += 2 + xlen;
    }
    for mask in [0x08, 0x10] {
        // FNAME, FCOMMENT: NUL-terminated strings.
        if flg & mask != 0 {
            while *b.get(pos)? != 0 {
                pos += 1;
            }
            pos += 1;
        }
    }
    if flg & 0x02 != 0 {
        pos += 2; // FHCRC
    }
    b.get(pos..)
}

/// A zlib header: CM=deflate (low nibble 8), window ≤ 32K (CINFO ≤ 7), and the
/// 2-byte header a multiple of 31 (the zlib check).
fn is_zlib(b: &[u8]) -> bool {
    b.len() >= 2
        && (b[0] & 0x0f) == 8
        && (b[0] >> 4) <= 7
        && ((b[0] as u16) << 8 | b[1] as u16).is_multiple_of(31)
}

/// An LSB-first bit reader over a DEFLATE stream.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    buf: u32,
    nbits: u32,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            buf: 0,
            nbits: 0,
        }
    }

    /// Read `n` (≤ 16) bits, LSB-first. `None` on end of input.
    fn bits(&mut self, n: u32) -> Option<u32> {
        while self.nbits < n {
            let byte = *self.data.get(self.pos)?;
            self.pos += 1;
            self.buf |= (byte as u32) << self.nbits;
            self.nbits += 8;
        }
        let mask = if n == 0 { 0 } else { (1u32 << n) - 1 };
        let val = self.buf & mask;
        self.buf >>= n;
        self.nbits -= n;
        Some(val)
    }

    /// Drop to the next byte boundary (before a stored block's length header).
    fn align(&mut self) {
        let drop = self.nbits % 8;
        self.buf >>= drop;
        self.nbits -= drop;
    }

    /// Take a whole byte (buffered bits first, then the underlying stream).
    fn take_byte(&mut self) -> Option<u8> {
        if self.nbits >= 8 {
            let b = (self.buf & 0xff) as u8;
            self.buf >>= 8;
            self.nbits -= 8;
            Some(b)
        } else {
            let b = *self.data.get(self.pos)?;
            self.pos += 1;
            Some(b)
        }
    }
}

/// A canonical Huffman table built from per-symbol code lengths, decoded
/// bit-at-a-time (the `puff.c` algorithm — compact and allocation-light).
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Huffman {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offsets = [0u16; 16];
        let mut sum = 0u16;
        for len in 1..16 {
            offsets[len] = sum;
            sum += counts[len];
        }
        let mut symbols = vec![0u16; sum as usize];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offsets[l as usize] as usize] = sym as u16;
                offsets[l as usize] += 1;
            }
        }
        Huffman { counts, symbols }
    }

    fn decode(&self, r: &mut BitReader) -> Option<u16> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for len in 1..16 {
            code |= r.bits(1)? as i32;
            let count = self.counts[len] as i32;
            if code - first < count {
                return self.symbols.get((index + (code - first)) as usize).copied();
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        None
    }
}

/// Length codes 257..=285: base length and extra bits (RFC 1951 §3.2.5).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// Distance codes 0..=29: base distance and extra bits.
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// Inflate a raw DEFLATE stream (block loop over stored / fixed / dynamic
/// Huffman blocks). `None` on malformed input or an output over [`INFLATE_MAX`].
fn inflate(data: &[u8]) -> Option<Vec<u8>> {
    let mut r = BitReader::new(data);
    let mut out: Vec<u8> = Vec::new();
    loop {
        let bfinal = r.bits(1)?;
        match r.bits(2)? {
            0 => inflate_stored(&mut r, &mut out)?,
            1 => inflate_block(&mut r, &mut out, &fixed_lit(), &fixed_dist())?,
            2 => {
                let (lit, dist) = dynamic_tables(&mut r)?;
                inflate_block(&mut r, &mut out, &lit, &dist)?;
            }
            _ => return None, // reserved BTYPE 3
        }
        if out.len() > INFLATE_MAX {
            return None;
        }
        if bfinal == 1 {
            return Some(out);
        }
    }
}

fn inflate_stored(r: &mut BitReader, out: &mut Vec<u8>) -> Option<()> {
    r.align();
    let len = r.take_byte()? as usize | ((r.take_byte()? as usize) << 8);
    let _nlen = (r.take_byte()?, r.take_byte()?); // one's-complement of len; unchecked
    for _ in 0..len {
        out.push(r.take_byte()?);
        if out.len() > INFLATE_MAX {
            return None;
        }
    }
    Some(())
}

fn inflate_block(
    r: &mut BitReader,
    out: &mut Vec<u8>,
    lit: &Huffman,
    dist: &Huffman,
) -> Option<()> {
    loop {
        let sym = lit.decode(r)?;
        match sym {
            256 => return Some(()), // end of block
            0..=255 => out.push(sym as u8),
            257..=285 => {
                let s = (sym - 257) as usize;
                let len = LENGTH_BASE[s] as usize + r.bits(LENGTH_EXTRA[s] as u32)? as usize;
                let dsym = dist.decode(r)? as usize;
                if dsym >= DIST_BASE.len() {
                    return None;
                }
                let d = DIST_BASE[dsym] as usize + r.bits(DIST_EXTRA[dsym] as u32)? as usize;
                if d == 0 || d > out.len() {
                    return None;
                }
                let start = out.len() - d;
                for i in 0..len {
                    out.push(out[start + i]); // overlapping copy (LZ77 run) is intentional
                }
            }
            _ => return None, // symbols 286/287 are reserved
        }
        if out.len() > INFLATE_MAX {
            return None;
        }
    }
}

/// The fixed literal/length code lengths (RFC 1951 §3.2.6).
fn fixed_lit() -> Huffman {
    let mut lengths = [0u8; 288];
    lengths[0..144].fill(8);
    lengths[144..256].fill(9);
    lengths[256..280].fill(7);
    lengths[280..288].fill(8);
    Huffman::new(&lengths)
}

/// The fixed distance code lengths: 30 codes of 5 bits each.
fn fixed_dist() -> Huffman {
    Huffman::new(&[5u8; 30])
}

/// Read a dynamic block's literal/length and distance Huffman tables from the
/// code-length code (RFC 1951 §3.2.7).
fn dynamic_tables(r: &mut BitReader) -> Option<(Huffman, Huffman)> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = r.bits(5)? as usize + 257;
    let hdist = r.bits(5)? as usize + 1;
    let hclen = r.bits(4)? as usize + 4;
    if hlit > 286 || hdist > 30 {
        return None;
    }
    let mut cl_lengths = [0u8; 19];
    for &slot in ORDER.iter().take(hclen) {
        cl_lengths[slot] = r.bits(3)? as u8;
    }
    let cl = Huffman::new(&cl_lengths);
    let total = hlit + hdist;
    let mut lengths: Vec<u8> = Vec::with_capacity(total);
    while lengths.len() < total {
        match cl.decode(r)? {
            sym @ 0..=15 => lengths.push(sym as u8),
            16 => {
                let prev = *lengths.last()?;
                let rep = r.bits(2)? as usize + 3;
                lengths.resize(lengths.len() + rep, prev);
            }
            17 => {
                let rep = r.bits(3)? as usize + 3;
                lengths.resize(lengths.len() + rep, 0);
            }
            18 => {
                let rep = r.bits(7)? as usize + 11;
                lengths.resize(lengths.len() + rep, 0);
            }
            _ => return None,
        }
    }
    if lengths.len() != total {
        return None; // a repeat ran past the declared table sizes
    }
    Some((
        Huffman::new(&lengths[..hlit]),
        Huffman::new(&lengths[hlit..]),
    ))
}

// ---------------------------------------------------------------------------
// Bitmap + HyperLogLog: Redis stores both as plain strings, so a value the
// user knows is a bitmap/HLL otherwise renders as a wall of hex. These give a
// bit view (with a local popcount — the `BITCOUNT`) and identify a HyperLogLog,
// estimating its cardinality client-side for the dense encoding.
// ---------------------------------------------------------------------------

/// Total set bits across `bytes` — the `BITCOUNT` of a bitmap, computed locally
/// (no round trip).
pub fn count_set_bits(bytes: &[u8]) -> u64 {
    bytes.iter().map(|b| b.count_ones() as u64).sum()
}

/// What [`hll_info`] learned about a HyperLogLog string.
pub struct HllInfo {
    /// `true` = dense encoding (fixed 16384×6-bit registers), `false` = sparse.
    pub dense: bool,
    /// Approximate cardinality. `Some` for a dense HLL (estimated from the
    /// registers here); `None` for sparse (decode it with `PFCOUNT`) or a
    /// truncated payload.
    pub estimate: Option<u64>,
}

/// Redis HyperLogLog header size and register geometry (`hyperloglog.c`).
const HLL_HDR: usize = 16;
const HLL_REGISTERS: usize = 16_384;
const HLL_BITS: usize = 6;
const HLL_MAX: u8 = 63;

/// Identify a Redis HyperLogLog by its `HYLL` magic and, for the dense encoding,
/// estimate its cardinality. `None` when the bytes aren't a HyperLogLog.
pub fn hll_info(bytes: &[u8]) -> Option<HllInfo> {
    if bytes.len() < HLL_HDR || &bytes[0..4] != b"HYLL" {
        return None;
    }
    // Header byte 4 is the encoding: 0 = dense, 1 = sparse.
    let dense = bytes[4] == 0;
    let estimate = dense
        .then(|| hll_dense_estimate(&bytes[HLL_HDR..]))
        .flatten();
    Some(HllInfo { dense, estimate })
}

/// Read the 6-bit register `j` from a dense HLL's packed register array
/// (little-endian bit packing, `HLL_DENSE_GET_REGISTER`).
fn hll_register(regs: &[u8], j: usize) -> u8 {
    let bit = j * HLL_BITS;
    let byte = bit / 8;
    let fb = bit % 8;
    let b0 = regs.get(byte).copied().unwrap_or(0) as u16;
    let b1 = regs.get(byte + 1).copied().unwrap_or(0) as u16;
    (((b0 >> fb) | (b1 << (8 - fb))) & HLL_MAX as u16) as u8
}

/// Estimate a dense HLL's cardinality from its registers (the classic
/// HyperLogLog estimator with linear-counting for the small range — close
/// enough for a "roughly enough data" read; `PFCOUNT` is exact). `None` if the
/// register array is truncated.
fn hll_dense_estimate(regs: &[u8]) -> Option<u64> {
    if regs.len() < HLL_REGISTERS * HLL_BITS / 8 {
        return None;
    }
    let m = HLL_REGISTERS as f64;
    let mut sum = 0.0f64;
    let mut zeros = 0u32;
    for j in 0..HLL_REGISTERS {
        let r = hll_register(regs, j);
        sum += 1.0 / (1u64 << r) as f64;
        if r == 0 {
            zeros += 1;
        }
    }
    let alpha = 0.7213 / (1.0 + 1.079 / m);
    let mut e = alpha * m * m / sum;
    // Linear counting when the raw estimate is in the small range and some
    // registers are still zero (matches Redis's low-cardinality correction).
    if e <= 2.5 * m && zeros != 0 {
        e = m * (m / zeros as f64).ln();
    }
    Some(e.round() as u64)
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

    #[test]
    fn pickle_dup_append_bomb_bails_without_oom() {
        // EMPTY_LIST then many DUP('2')+APPEND('a') pairs: each doubles the
        // node count. The node budget must turn this into a clean `None`
        // (fall back to hex) rather than exhausting memory. 30 pairs would be
        // 2^30 nodes unguarded.
        let mut bytes = vec![0x80, 0x02, b']'];
        for _ in 0..30 {
            bytes.push(b'2');
            bytes.push(b'a');
        }
        bytes.push(b'.');
        assert!(decode_pickle(&bytes).is_none());
    }

    #[test]
    fn pickle_binget_missing_memo_bails() {
        // BINGET ('h') index 5 with an empty memo is malformed: bail rather
        // than fabricating a Null.
        assert!(decode_pickle(b"\x80\x02h\x05.").is_none());
    }

    #[test]
    fn protobuf_rejects_overlong_varint() {
        // A 10-byte varint whose final byte sets bits above 0x01 would overflow
        // a u64; reject it instead of silently truncating.
        let overlong = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert!(varint(&mut Cur::new(&overlong)).is_none());
        // The boundary value 0x01 in the 10th byte is still a valid u64.
        let ok = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert!(varint(&mut Cur::new(&ok)).is_some());
    }

    #[test]
    fn deeply_nested_render_does_not_overflow() {
        // A decoder can build a value tree deeper than any stack tolerates
        // (pickle's build loop is iterative, no depth cap); rendering it must
        // not recurse unbounded. The `Decoded::write` depth guard truncates.
        let mut d = Decoded::Int(1);
        for _ in 0..5_000 {
            d = Decoded::Array(vec![d]);
        }
        let out = d.render(); // must not stack-overflow
        assert!(out.contains('…'), "depth guard should truncate the render");
    }

    #[test]
    fn deeply_nested_protobuf_does_not_overflow() {
        fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
            loop {
                let mut b = (v & 0x7f) as u8;
                v >>= 7;
                if v != 0 {
                    b |= 0x80;
                }
                out.push(b);
                if v == 0 {
                    break;
                }
            }
        }
        // Nest thousands of length-delimited wrappers, each a message holding
        // the next. Without the render depth guard this overflows the stack.
        let mut inner = vec![0x08u8, 0x01]; // innermost: field 1 varint 1
        for _ in 0..2_000 {
            let mut wrapped = vec![0x0au8]; // field 1, wire type 2 (length-delimited)
            pb_varint(inner.len() as u64, &mut wrapped);
            wrapped.extend_from_slice(&inner);
            inner = wrapped;
        }
        let _ = decode_protobuf(&inner); // must not stack-overflow
    }

    /// Decode an ASCII-hex string into bytes (test helper for real fixtures).
    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decompress_inflates_a_real_gzip_stream() {
        // `printf 'hello, redis world! hello, redis world!' | gzip -n`
        let gz = unhex("1f8b0800000000000003cb48cdc9c9d751284a4dc92c5628cf2fca495154c8c014030020f4948827000000");
        let out = decompress(&gz).unwrap();
        assert_eq!(out, b"hello, redis world! hello, redis world!");
    }

    #[test]
    fn decompress_inflates_a_real_zlib_stream() {
        // `zlib.compress(b'hello, redis world! hello, redis world!')`
        let zl = unhex("789ccb48cdc9c9d751284a4dc92c5628cf2fca495154c8c014030018180de1");
        let out = decompress(&zl).unwrap();
        assert_eq!(out, b"hello, redis world! hello, redis world!");
    }

    #[test]
    fn decompress_rejects_non_compressed_bytes() {
        assert!(decompress(b"not compressed at all").is_none());
        assert!(decompress(b"").is_none());
        // A gzip magic with a truncated body must not panic — just fail.
        assert!(decompress(b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03\xff").is_none());
    }

    #[test]
    fn timestamp_detects_units_and_renders_utc() {
        // 1700000000 s = 2023-11-14 22:13:20 UTC.
        let s = decode_timestamp(b"1700000000").unwrap();
        assert!(s.starts_with("2023-11-14 22:13:20 UTC"), "{s}");
        assert!(s.contains("seconds"), "{s}");
        // Same instant in milliseconds.
        let ms = decode_timestamp(b"1700000000000").unwrap();
        assert!(ms.starts_with("2023-11-14 22:13:20 UTC"), "{ms}");
        assert!(ms.contains("millis"), "{ms}");
        // The Unix epoch itself, as an 8-byte big-endian blob.
        let epoch = decode_timestamp(&1_700_000_000i64.to_be_bytes()).unwrap();
        assert!(epoch.starts_with("2023-11-14"), "{epoch}");
    }

    #[test]
    fn timestamp_rejects_small_or_non_integers() {
        // Too small to be a plausible epoch (pre-1973).
        assert!(decode_timestamp(b"42").is_none());
        // Not an integer at all.
        assert!(decode_timestamp(b"hello").is_none());
        assert!(decode_timestamp(b"").is_none());
    }

    #[test]
    fn count_set_bits_matches_bitcount() {
        assert_eq!(count_set_bits(b""), 0);
        assert_eq!(count_set_bits(&[0x00]), 0);
        assert_eq!(count_set_bits(&[0xff]), 8);
        assert_eq!(count_set_bits(&[0b1010_1010, 0b0000_0001]), 5);
    }

    #[test]
    fn hll_info_detects_magic_and_encoding() {
        // Not a HyperLogLog.
        assert!(hll_info(b"just a string value").is_none());
        assert!(hll_info(b"HYL").is_none()); // too short

        // A well-formed dense HLL with all-zero registers: 16-byte header
        // (magic + dense flag) then 12288 zero register bytes. Linear counting
        // over an all-empty register set estimates 0.
        let mut hll = Vec::new();
        hll.extend_from_slice(b"HYLL");
        hll.push(0); // dense
        hll.extend_from_slice(&[0u8; 11]); // rest of the header
        hll.extend_from_slice(&[0u8; HLL_REGISTERS * HLL_BITS / 8]);
        let info = hll_info(&hll).unwrap();
        assert!(info.dense);
        assert_eq!(info.estimate, Some(0));

        // Sparse encoding: identified, but not estimated here.
        let mut sparse = hll.clone();
        sparse[4] = 1; // sparse flag
        let info = hll_info(&sparse).unwrap();
        assert!(!info.dense);
        assert_eq!(info.estimate, None);
    }
}
