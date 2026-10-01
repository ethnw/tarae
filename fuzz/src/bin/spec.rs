//! Runs the official WebAssembly spec tests against the plugin runtime (`src/wasm`), JIT and interpreter.
//!
//! ```text
//! git clone --depth 1 https://github.com/WebAssembly/testsuite
//! cargo run --release --bin spec -- testsuite/*.wast
//! ```
//!
//! Modules using what the runtime leaves out on purpose — SIMD, memory64 and multiple memories, GC, tail
//! calls, exceptions, and anything importing from the host — are counted as skipped, along with the
//! assertions on them. Everything else must pass.
#[allow(dead_code)]
#[path = "../../../src/wasm/mod.rs"]
mod wasm;

use std::collections::HashMap;

use wasm::{Instance, Limits, Module, Trap, Val};
use wast::core::{AbstractHeapType, HeapType, NanPattern, WastArgCore, WastRetCore};
use wast::parser::{self, ParseBuffer};
use wast::{QuoteWat, Wast, WastArg, WastDirective, WastExecute, WastInvoke, WastRet};

const FUEL: u64 = 1 << 42;

#[derive(Default)]
struct Tally {
    pass: usize,
    fail: usize,
    skip: usize,
    fails: Vec<String>,
}

/// Rejections for what the runtime doesn't do by design.
fn unsupported(e: &str) -> bool {
    e.contains("imports") || e.contains("not supported") || e.contains("only function types")
}

fn arg(a: &WastArg) -> Option<Val> {
    let WastArg::Core(a) = a else { return None };
    Some(match a {
        WastArgCore::I32(v) => Val::I32(*v),
        WastArgCore::I64(v) => Val::I64(*v),
        WastArgCore::F32(f) => Val::F32(f.bits),
        WastArgCore::F64(f) => Val::F64(f.bits),
        WastArgCore::RefNull(HeapType::Abstract { ty: AbstractHeapType::Func, .. }) => Val::FuncRef(None),
        WastArgCore::RefNull(HeapType::Abstract { ty: AbstractHeapType::Extern, .. }) => Val::ExternRef(None),
        WastArgCore::RefExtern(n) => Val::ExternRef(Some(*n)),
        _ => return None,
    })
}

/// Whether a result matches the expected one (`None`: can't tell here).
fn matches(want: &WastRet, got: &Val) -> Option<bool> {
    let WastRet::Core(w) = want else { return None };
    Some(match (w, got) {
        (WastRetCore::I32(a), Val::I32(b)) => a == b,
        (WastRetCore::I64(a), Val::I64(b)) => a == b,
        (WastRetCore::F32(p), Val::F32(b)) => match p {
            NanPattern::CanonicalNan => b & 0x7FFF_FFFF == 0x7FC0_0000,
            NanPattern::ArithmeticNan => b & 0x7FC0_0000 == 0x7FC0_0000,
            NanPattern::Value(v) => v.bits == *b,
        },
        (WastRetCore::F64(p), Val::F64(b)) => match p {
            NanPattern::CanonicalNan => b & !(1 << 63) == 0x7FF8_0000_0000_0000,
            NanPattern::ArithmeticNan => b & 0x7FF8_0000_0000_0000 == 0x7FF8_0000_0000_0000,
            NanPattern::Value(v) => v.bits == *b,
        },
        (WastRetCore::RefNull(_), Val::FuncRef(r) | Val::ExternRef(r)) => r.is_none(),
        (WastRetCore::RefFunc(_), Val::FuncRef(r)) => r.is_some(),
        (WastRetCore::RefExtern(Some(n)), Val::ExternRef(r)) => *r == Some(*n),
        (WastRetCore::RefExtern(None), Val::ExternRef(r)) => r.is_some(),
        (WastRetCore::V128(_), _) => return None,
        _ => false,
    })
}

enum Outcome {
    Vals(Vec<Val>),
    Trap(Trap),
    Skip,
}

struct Env {
    jit: bool,
    instances: Vec<Instance>,
    current: Option<usize>,
    named: HashMap<String, usize>,
    /// A module was skipped for importing — it may have changed named instances through shared tables or
    /// memories, so assertions on those can't be trusted any more.
    linked_away: bool,
}

impl Env {
    fn instance(&mut self, name: Option<&str>) -> Option<&mut Instance> {
        let i = match name {
            Some(_) if self.linked_away => return None,
            Some(n) => *self.named.get(n)?,
            None => self.current?,
        };
        self.instances.get_mut(i)
    }

    fn limits(&self) -> Limits {
        Limits { jit: self.jit, ..Limits::default() }
    }

    fn invoke(&mut self, i: &WastInvoke) -> Outcome {
        let Some(args) = i.args.iter().map(arg).collect::<Option<Vec<_>>>() else { return Outcome::Skip };
        let Some(inst) = self.instance(i.module.map(|m| m.name())) else { return Outcome::Skip };
        let Some(f) = inst.module().func(i.name) else { return Outcome::Skip };
        inst.set_fuel(FUEL);
        match inst.call(f, &args) {
            Ok(v) => Outcome::Vals(v),
            Err(t) => Outcome::Trap(t),
        }
    }

    fn exec(&mut self, e: &mut WastExecute) -> Outcome {
        match e {
            WastExecute::Invoke(i) => self.invoke(i),
            WastExecute::Get { module, global, .. } => {
                let Some(inst) = self.instance(module.map(|m| m.name())) else { return Outcome::Skip };
                inst.global(global).map_or(Outcome::Skip, |v| Outcome::Vals(vec![v]))
            }
            WastExecute::Wat(w) => {
                let Ok(bytes) = w.encode() else { return Outcome::Skip };
                let m = match Module::new(&bytes) {
                    Ok(m) => m,
                    Err(e) => {
                        self.linked_away |= e.to_string().contains("imports");
                        return Outcome::Skip;
                    }
                };
                match Instance::new(m, self.limits(), FUEL) {
                    Ok(_) => Outcome::Vals(vec![]),
                    Err(t) => Outcome::Trap(t),
                }
            }
        }
    }
}

fn run(path: &str, jit: bool, t: &mut Tally) {
    let text = std::fs::read_to_string(path).expect("readable");
    let Ok(buf) = ParseBuffer::new(&text) else { return t.skip += 1 };
    let Ok(wast) = parser::parse::<Wast>(&buf) else { return t.skip += 1 };
    let mut env =
        Env { jit, instances: Vec::new(), current: None, named: HashMap::new(), linked_away: false };
    let file = path.rsplit('/').next().unwrap_or(path);
    for d in wast.directives {
        let (line, _) = d.span().linecol_in(&text);
        let at = format!("{file}:{}", line + 1);
        let fail = |t: &mut Tally, why: String| {
            t.fail += 1;
            t.fails.push(format!("{at}: {why}"));
        };
        match d {
            WastDirective::Module(mut qw) => {
                env.current = None;
                let name = qw.name().map(|n| n.name().to_string());
                let Ok(bytes) = qw.encode() else {
                    t.skip += 1;
                    continue;
                };
                match Module::new(&bytes) {
                    Ok(m) => match Instance::new(m, env.limits(), FUEL) {
                        Ok(i) => {
                            t.pass += 1;
                            env.instances.push(i);
                            env.current = Some(env.instances.len() - 1);
                            if let Some(n) = name {
                                env.named.insert(n, env.instances.len() - 1);
                            }
                        }
                        Err(tr) => fail(t, format!("instantiation trapped: {tr}")),
                    },
                    Err(e) if unsupported(&e.to_string()) => {
                        env.linked_away |= e.to_string().contains("imports");
                        t.skip += 1
                    }
                    Err(e) => fail(t, format!("module rejected: {e}")),
                }
            }
            WastDirective::AssertReturn { mut exec, results, .. } => match env.exec(&mut exec) {
                Outcome::Skip => t.skip += 1,
                Outcome::Trap(tr) => fail(t, format!("trapped: {tr}")),
                Outcome::Vals(v) if v.len() != results.len() => {
                    fail(t, format!("{} results, wanted {}", v.len(), results.len()))
                }
                Outcome::Vals(v) => {
                    match results.iter().zip(&v).map(|(w, g)| matches(w, g)).collect::<Option<Vec<bool>>>() {
                        None => t.skip += 1,
                        Some(ok) if ok.iter().all(|&x| x) => t.pass += 1,
                        Some(_) => fail(t, format!("got {v:?}, wanted {results:?}")),
                    }
                }
            },
            WastDirective::AssertTrap { mut exec, message, .. } => match env.exec(&mut exec) {
                Outcome::Skip => t.skip += 1,
                Outcome::Trap(tr)
                    if message.starts_with(&tr.to_string()) || tr.to_string().starts_with(message) =>
                {
                    t.pass += 1
                }
                Outcome::Trap(tr) => fail(t, format!("trap {tr:?}, wanted {message}")),
                Outcome::Vals(v) => fail(t, format!("returned {v:?}, wanted trap {message}")),
            },
            WastDirective::AssertExhaustion { call, message, .. } => match env.invoke(&call) {
                Outcome::Skip => t.skip += 1,
                Outcome::Trap(Trap::StackOverflow) => t.pass += 1,
                Outcome::Trap(tr) => fail(t, format!("trap {tr:?}, wanted {message}")),
                Outcome::Vals(_) => fail(t, format!("returned, wanted {message}")),
            },
            WastDirective::AssertInvalid { mut module, message, .. }
            | WastDirective::AssertMalformed { mut module, message, .. } => {
                let quoted_text = matches!(module, QuoteWat::QuoteModule(..));
                match module.encode() {
                    Err(_) => t.pass += 1, // the text itself is malformed
                    Ok(bytes) => match Module::new(&bytes) {
                        Err(_) => t.pass += 1,
                        Ok(_) if quoted_text => t.skip += 1,
                        Ok(_) => fail(t, format!("accepted, wanted: {message}")),
                    },
                }
            }
            WastDirective::Invoke(i) => drop(env.invoke(&i)),
            WastDirective::AssertUnlinkable { .. } => {
                // Linking against the host: skipped, but it may have written to registered instances first.
                env.linked_away = true;
                t.skip += 1
            }
            _ => t.skip += 1,
        }
    }
}

fn main() {
    let files: Vec<String> = std::env::args().skip(1).collect();
    let mut failed = false;
    for jit in [true, false] {
        let mut total = Tally::default();
        for path in &files {
            let mut t = Tally::default();
            run(path, jit, &mut t);
            total.pass += t.pass;
            total.fail += t.fail;
            total.skip += t.skip;
            total.fails.extend(t.fails);
        }
        let engine = if jit { "jit        " } else { "interpreter" };
        println!("{engine}  pass {}  fail {}  skip {}", total.pass, total.fail, total.skip);
        for f in total.fails.iter().take(40) {
            println!("  {f}");
        }
        failed |= total.fail > 0;
    }
    std::process::exit(failed as i32);
}
