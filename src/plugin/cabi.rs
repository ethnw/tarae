//! The part of the WebAssembly component model's *canonical ABI* that tarae's plugin API uses: bool, u64,
//! string, list (and `list<u8>`, as bytes), option, record/tuple and variant, laid out in a plugin's memory the way wit-bindgen expects.
//!
//! Values going in are stored with the plugin's own `cabi_realloc`; values coming out are read back with
//! every pointer, length and alignment checked — a plugin can answer with garbage, never crash tarae. What
//! reading an answer builds is budgeted too: lists and strings may point at the same memory over and over,
//! so an answer could otherwise ask for far more than the plugin's memory holds.

use crate::wasm::{Instance, Val};

/// A WIT type (only the ones the API uses).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum T {
    Bool,
    U64,
    Str,
    /// `list<u8>`: copied as one run of bytes.
    Bytes,
    List(Box<T>),
    Opt(Box<T>),
    /// A record or a tuple: fields in order.
    Rec(Vec<T>),
    /// Cases in order, each with a payload or not.
    Var(Vec<Option<T>>),
}

/// A value of a `T`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum V {
    Bool(bool),
    U64(u64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<V>),
    Opt(Option<Box<V>>),
    Rec(Vec<V>),
    Var(u32, Option<Box<V>>),
}

fn round_up(n: u32, align: u32) -> u32 {
    n.div_ceil(align) * align
}

/// Bytes of a variant's discriminant.
fn disc_size(cases: usize) -> u32 {
    match cases {
        0..=256 => 1,
        257..=65536 => 2,
        _ => 4,
    }
}

impl T {
    pub fn align(&self) -> u32 {
        match self {
            T::Bool => 1,
            T::U64 => 8,
            T::Str | T::Bytes | T::List(_) => 4,
            T::Opt(t) => t.align(),
            T::Rec(fs) => fs.iter().map(T::align).max().unwrap_or(1),
            T::Var(cs) => cs.iter().flatten().map(T::align).fold(disc_size(cs.len()), u32::max),
        }
    }

    pub fn size(&self) -> u32 {
        match self {
            T::Bool => 1,
            T::U64 | T::Str | T::Bytes | T::List(_) => 8,
            T::Opt(t) => round_up(self.payload_at() + t.size(), self.align()),
            T::Rec(fs) => {
                round_up(fs.iter().fold(0, |at, f| round_up(at, f.align()) + f.size()), self.align())
            }
            T::Var(cs) => {
                let payload = cs.iter().flatten().map(T::size).max().unwrap_or(0);
                round_up(self.payload_at() + payload, self.align())
            }
        }
    }

    /// Where an option's or variant's payload starts.
    fn payload_at(&self) -> u32 {
        match self {
            T::Opt(t) => round_up(1, t.align()),
            T::Var(cs) => round_up(disc_size(cs.len()), cs.iter().flatten().map(T::align).max().unwrap_or(1)),
            _ => 0,
        }
    }
}

/// The most an answer may make tarae build: string bytes, plus 64 per list element.
const ANSWER_BUDGET: u64 = 64 << 20;

/// Reading and writing values in one plugin instance's memory.
pub struct Mem<'a> {
    inst: &'a mut Instance,
    /// The plugin's `cabi_realloc` export.
    realloc: u32,
    /// What reading may still build (see `ANSWER_BUDGET`).
    left: std::cell::Cell<u64>,
}

impl<'a> Mem<'a> {
    pub fn new(inst: &'a mut Instance, realloc: u32) -> Mem<'a> {
        Mem { inst, realloc, left: std::cell::Cell::new(ANSWER_BUDGET) }
    }

    fn spend(&self, n: u64) -> Result<(), String> {
        let left = self.left.get().checked_sub(n).ok_or("its answer is too big")?;
        self.left.set(left);
        Ok(())
    }

    /// `size` fresh bytes aligned to `align`, from the plugin's allocator.
    pub fn alloc(&mut self, align: u32, size: u32) -> Result<u32, String> {
        let args = [Val::I32(0), Val::I32(0), Val::I32(align as i32), Val::I32(size as i32)];
        match self.inst.call(self.realloc, &args).map_err(|t| t.to_string())?[..] {
            [Val::I32(p)] if (p as u32).is_multiple_of(align) => Ok(p as u32),
            _ => Err("its allocator returned a misaligned pointer".into()),
        }
    }

    fn bytes(&self, at: u32, n: u32) -> Result<&[u8], String> {
        let range = at as usize..at as usize + n as usize;
        self.inst.memory().get(range).ok_or_else(|| "it answered with a pointer outside its memory".into())
    }

    fn put(&mut self, at: u32, bytes: &[u8]) -> Result<(), String> {
        let range = at as usize..at as usize + bytes.len();
        let dst =
            self.inst.memory_mut().get_mut(range).ok_or("its allocator returned memory it doesn't have")?;
        dst.copy_from_slice(bytes);
        Ok(())
    }

    fn u32(&self, at: u32) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.bytes(at, 4)?.try_into().expect("4 bytes")))
    }

    /// A (pointer, length) pair at `at`, for `n` elements of `size` bytes aligned to `align` — checked to be
    /// inside memory, so a made-up length can't make tarae allocate for it.
    fn span(&self, at: u32, size: u32, align: u32) -> Result<(u32, u32), String> {
        let (p, n) = (self.u32(at)?, self.u32(at + 4)?);
        let bytes = n as u64 * size as u64;
        if !p.is_multiple_of(align) || p as u64 + bytes > self.inst.memory().len() as u64 {
            return Err("it answered with a list outside its memory".into());
        }
        self.spend(if size == 1 && align == 1 { n as u64 } else { n as u64 * 64 })?;
        Ok((p, n))
    }

    /// Lays out a text, bytes or a list in fresh memory: its (pointer, length) — as an argument passed flat
    /// takes it (the plugin owns that memory then, and frees it).
    pub fn lay(&mut self, t: &T, v: &V) -> Result<(u32, u32), String> {
        match (t, v) {
            (T::Str, V::Str(s)) => {
                let len = u32::try_from(s.len()).map_err(|_| "text too long for a plugin")?;
                let p = self.alloc(1, len)?;
                self.put(p, s.as_bytes())?;
                Ok((p, len))
            }
            (T::Bytes, V::Bytes(b)) => {
                let len = u32::try_from(b.len()).map_err(|_| "bytes too long for a plugin")?;
                let p = self.alloc(1, len)?;
                self.put(p, b)?;
                Ok((p, len))
            }
            (T::List(e), V::List(vs)) => {
                let len = u32::try_from(vs.len()).map_err(|_| "list too long for a plugin")?;
                let bytes = len.checked_mul(e.size()).ok_or("list too long for a plugin")?;
                let p = self.alloc(e.align(), bytes)?;
                for (i, v) in vs.iter().enumerate() {
                    self.store(e, v, p + i as u32 * e.size())?;
                }
                Ok((p, len))
            }
            _ => unreachable!("tarae laid out a value that isn't a text or a list: {v:?} for {t:?}"),
        }
    }

    /// Stores `v` (of type `t`) at `at`.
    pub fn store(&mut self, t: &T, v: &V, at: u32) -> Result<(), String> {
        match (t, v) {
            (T::Bool, V::Bool(b)) => self.put(at, &[*b as u8]),
            (T::U64, V::U64(n)) => self.put(at, &n.to_le_bytes()),
            (T::Str, V::Str(_)) | (T::Bytes, V::Bytes(_)) | (T::List(_), V::List(_)) => {
                let (p, len) = self.lay(t, v)?;
                self.put(at, &[p.to_le_bytes(), len.to_le_bytes()].concat())
            }
            (T::Opt(e), V::Opt(o)) => {
                self.put(at, &[o.is_some() as u8])?;
                match o {
                    Some(v) => self.store(e, v, at + t.payload_at()),
                    None => Ok(()),
                }
            }
            (T::Rec(fs), V::Rec(vs)) if fs.len() == vs.len() => {
                let mut off = 0;
                for (f, v) in fs.iter().zip(vs) {
                    off = round_up(off, f.align());
                    self.store(f, v, at + off)?;
                    off += f.size();
                }
                Ok(())
            }
            (T::Var(cs), V::Var(c, payload)) => {
                self.put(at, &c.to_le_bytes()[..disc_size(cs.len()) as usize])?;
                match (cs.get(*c as usize), payload) {
                    (Some(Some(e)), Some(v)) => self.store(e, v, at + t.payload_at()),
                    (Some(None), None) => Ok(()),
                    _ => unreachable!("tarae built a variant that doesn't match its type"),
                }
            }
            _ => unreachable!("tarae built a value that doesn't match its type: {v:?} for {t:?}"),
        }
    }

    /// Reads a value of type `t` at `at`.
    pub fn load(&self, t: &T, at: u32) -> Result<V, String> {
        Ok(match t {
            T::Bool => V::Bool(self.bytes(at, 1)?[0] != 0),
            T::U64 => V::U64(u64::from_le_bytes(self.bytes(at, 8)?.try_into().expect("8 bytes"))),
            T::Str => {
                let (p, n) = self.span(at, 1, 1)?;
                let s = std::str::from_utf8(self.bytes(p, n)?)
                    .map_err(|_| "it answered with text that isn't UTF-8")?;
                V::Str(s.to_string())
            }
            T::Bytes => {
                let (p, n) = self.span(at, 1, 1)?;
                V::Bytes(self.bytes(p, n)?.to_vec())
            }
            T::List(e) => {
                let (p, n) = self.span(at, e.size(), e.align())?;
                V::List((0..n).map(|i| self.load(e, p + i * e.size())).collect::<Result<_, _>>()?)
            }
            T::Opt(e) => match self.bytes(at, 1)?[0] {
                0 => V::Opt(None),
                1 => V::Opt(Some(Box::new(self.load(e, at + t.payload_at())?))),
                _ => return Err("it answered with a malformed option".into()),
            },
            T::Rec(fs) => {
                let mut off = 0;
                let mut vs = Vec::with_capacity(fs.len());
                for f in fs {
                    off = round_up(off, f.align());
                    vs.push(self.load(f, at + off)?);
                    off += f.size();
                }
                V::Rec(vs)
            }
            T::Var(cs) => {
                let d = self.bytes(at, disc_size(cs.len()))?;
                let c = d.iter().rev().fold(0, |acc, &b| acc << 8 | b as u32);
                match cs.get(c as usize) {
                    Some(Some(e)) => V::Var(c, Some(Box::new(self.load(e, at + t.payload_at())?))),
                    Some(None) => V::Var(c, None),
                    None => return Err("it answered with an unknown case".into()),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> T {
        T::Str
    }

    /// The layouts the canonical ABI gives these types (sizes and alignments from its spec's rules).
    #[test]
    fn layouts_follow_the_canonical_abi() {
        let opt_str = T::Opt(Box::new(s()));
        assert_eq!((opt_str.size(), opt_str.align()), (12, 4));
        let opt_u64 = T::Opt(Box::new(T::U64));
        assert_eq!((opt_u64.size(), opt_u64.align()), (16, 8));
        // record { a: bool, b: u64, c: string } → a at 0, b at 8, c at 16
        let r = T::Rec(vec![T::Bool, T::U64, s()]);
        assert_eq!((r.size(), r.align()), (24, 8));
        // variant { a(string), b, c(u64) } → discriminant u8, payload at 8
        let v = T::Var(vec![Some(s()), None, Some(T::U64)]);
        assert_eq!((v.size(), v.align(), v.payload_at()), (16, 8, 8));
        let wide = T::Var(vec![None; 300]);
        assert_eq!((wide.size(), wide.align()), (2, 2));
        assert_eq!(T::Rec(vec![]).size(), 0);
    }

    /// A module with a bump allocator, standing in for a plugin.
    fn instance() -> (Instance, u32) {
        let wat = r#"(module
            (memory (export "memory") 1)
            (global $next (mut i32) (i32.const 1024))
            (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
              (local $p i32)
              ;; round up to the alignment, hand out, bump
              (local.set $p (i32.and (i32.add (global.get $next) (i32.sub (local.get 2) (i32.const 1)))
                                     (i32.sub (i32.const 0) (local.get 2))))
              (global.set $next (i32.add (local.get $p) (local.get 3)))
              (local.get $p)))"#;
        let m = crate::wasm::Module::new(&wat::parse_str(wat).unwrap()).unwrap();
        let realloc = m.func("cabi_realloc").unwrap();
        (Instance::new(m, Default::default(), 1 << 30).unwrap(), realloc)
    }

    #[test]
    fn values_round_trip() {
        let (mut inst, realloc) = instance();
        let mut mem = Mem::new(&mut inst, realloc);
        let t = T::Rec(vec![
            s(),
            T::List(Box::new(T::Rec(vec![T::U64, T::U64]))),
            T::Opt(Box::new(s())),
            T::Opt(Box::new(s())),
            T::Bool,
            T::Var(vec![Some(s()), None, Some(T::List(Box::new(s())))]),
        ]);
        let v = V::Rec(vec![
            V::Str("héllo".into()),
            V::List(vec![V::Rec(vec![V::U64(1), V::U64(u64::MAX)]), V::Rec(vec![V::U64(3), V::U64(4)])]),
            V::Opt(Some(Box::new(V::Str(String::new())))),
            V::Opt(None),
            V::Bool(true),
            V::Var(2, Some(Box::new(V::List(vec![V::Str("a".into()), V::Str("bc".into())])))),
        ]);
        let at = mem.alloc(t.align(), t.size()).unwrap();
        mem.store(&t, &v, at).unwrap();
        assert_eq!(mem.load(&t, at).unwrap(), v);
    }

    /// Many list entries pointing at the same big string: bounded by the budget, not by memory.
    #[test]
    fn aliased_answers_are_budgeted() {
        let (mut inst, realloc) = instance();
        let mut mem = Mem::new(&mut inst, realloc);
        // 20 KB of text, and a list of 4000 strings all pointing at it: 80 MB if built
        let text = mem.alloc(1, 20_000).unwrap();
        let list = mem.alloc(4, 4000 * 8).unwrap();
        for i in 0..4000 {
            mem.put(list + i * 8, &[text.to_le_bytes(), 20_000u32.to_le_bytes()].concat()).unwrap();
        }
        let head = mem.alloc(4, 8).unwrap();
        mem.put(head, &[list.to_le_bytes(), 4000u32.to_le_bytes()].concat()).unwrap();
        assert_eq!(mem.load(&T::List(Box::new(s())), head), Err("its answer is too big".into()));
    }

    #[test]
    fn garbage_is_an_error_not_a_crash() {
        let (mut inst, realloc) = instance();
        let words = |inst: &mut Instance, at: usize, ws: &[u32]| {
            for (i, w) in ws.iter().enumerate() {
                inst.memory_mut()[at + i * 4..at + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
            }
        };
        let list = T::List(Box::new(T::Rec(vec![T::U64, s()])));
        // a list claiming four billion elements
        words(&mut inst, 0, &[64, u32::MAX]);
        assert!(Mem::new(&mut inst, realloc).load(&list, 0).is_err());
        // a string past the end of memory, a misaligned list, text that isn't UTF-8
        words(&mut inst, 0, &[65530, 10]);
        assert!(Mem::new(&mut inst, realloc).load(&s(), 0).is_err());
        words(&mut inst, 0, &[3, 1]);
        assert!(Mem::new(&mut inst, realloc).load(&list, 0).is_err());
        words(&mut inst, 0, &[8, 2, 0xFFFF]);
        assert!(Mem::new(&mut inst, realloc).load(&s(), 0).is_err());
        // an unknown variant case, a malformed option
        inst.memory_mut()[0] = 7;
        assert!(Mem::new(&mut inst, realloc).load(&T::Var(vec![None, None]), 0).is_err());
        assert!(Mem::new(&mut inst, realloc).load(&T::Opt(Box::new(T::Bool)), 0).is_err());
    }
}
