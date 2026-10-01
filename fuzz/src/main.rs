//! Differential fuzzer for the plugin runtime (`src/wasm`).
//!
//! Random valid modules (wasm-smith, limited to what the runtime supports) run on tarae's JIT, tarae's
//! interpreter, and wasmi. JIT and interpreter must agree exactly on every call — results, traps, memory —
//! fuel and stack limits included. Against wasmi, resource-limit outcomes are skipped (engines count fuel
//! differently); when wasmi disagrees, wasmtime replays the calls and decides who's right. Each module's byte
//! mutations also go through the decoder (no panics allowed) and, if still valid, through everything.
//!
//! ```text
//! fuzz <seconds> [start-seed] [notraps]   run; `notraps` generates trap-free code (reaches deeper)
//! fuzz repro <file.wasm> jit|interp|all   rerun one module
//! fuzz wat <file.wasm>                    print it as text
//! ```
//!
//! Findings are saved to `found/`; the module in flight is always in `last.wasm`, so a crash leaves it too.
//! wasmi's own mistakes — wrong answers wasmtime refutes, and its panics — are counted apart, not found.
//! Several processes with different seeds in different directories use every core.
#[allow(dead_code)]
#[path = "../../src/wasm/mod.rs"]
mod wasm;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use arbitrary::Unstructured;
use wasm::{Instance, Limits, Module, Trap, Val};

const FUEL: u64 = 2_000_000;

static NO_TRAPS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn config() -> wasm_smith::Config {
    wasm_smith::Config {
        disallow_traps: NO_TRAPS.load(std::sync::atomic::Ordering::Relaxed),
        max_imports: 0,
        max_memories: 1,
        max_memory32_bytes: 16 << 16,
        max_tables: 3,
        max_table_elements: 200,
        max_funcs: 24,
        max_instructions: 1500,
        max_nesting_depth: 12,
        simd_enabled: false,
        relaxed_simd_enabled: false,
        threads_enabled: false,
        shared_everything_threads_enabled: false,
        exceptions_enabled: false,
        gc_enabled: false,
        tail_call_enabled: false,
        memory64_enabled: false,
        extended_const_enabled: false,
        custom_page_sizes_enabled: false,
        wide_arithmetic_enabled: false,
        custom_descriptors_enabled: false,
        multi_value_enabled: true,
        bulk_memory_enabled: true,
        reference_types_enabled: true,
        saturating_float_to_int_enabled: true,
        sign_extension_ops_enabled: true,
        canonicalize_nans: true,
        export_everything: true,
        generate_custom_sections: false,
        ..wasm_smith::Config::default()
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// What a call did, comparable across engines.
#[derive(Debug, Clone, PartialEq)]
enum Out {
    Vals(Vec<Val>),
    Trap(Trap),
}

fn ours(bytes: &[u8], jit: bool) -> Result<Instance, String> {
    let m = Module::new(bytes).map_err(|e| format!("rejected: {e}"))?;
    if jit && m_has_no_jit(&m) {
        return Err("no jit".into());
    }
    let limits =
        Limits { stack_slots: 1 << 16, call_depth: 500, memory_bytes: 64 << 20, jit, ..Limits::default() };
    Instance::new(m, limits, FUEL).map_err(|t| format!("instantiate trapped: {t:?}"))
}

fn m_has_no_jit(_m: &Module) -> bool {
    false
}

fn wasmi_val(v: &Val) -> wasmi::Val {
    match *v {
        Val::I32(x) => wasmi::Val::I32(x),
        Val::I64(x) => wasmi::Val::I64(x),
        Val::F32(b) => wasmi::Val::F32(wasmi::F32::from_bits(b)),
        Val::F64(b) => wasmi::Val::F64(wasmi::F64::from_bits(b)),
        Val::FuncRef(_) => wasmi::Val::FuncRef(wasmi::Nullable::Null),
        Val::ExternRef(_) => wasmi::Val::ExternRef(wasmi::Nullable::Null),
    }
}

fn from_wasmi(v: &wasmi::Val) -> Val {
    match v {
        wasmi::Val::I32(x) => Val::I32(*x),
        wasmi::Val::I64(x) => Val::I64(*x),
        wasmi::Val::F32(f) => Val::F32(f.to_bits()),
        wasmi::Val::F64(f) => Val::F64(f.to_bits()),
        wasmi::Val::FuncRef(r) => Val::FuncRef(if r.is_null() { None } else { Some(0) }),
        wasmi::Val::ExternRef(r) => Val::ExternRef(if r.is_null() { None } else { Some(0) }),
        _ => Val::I32(i32::MIN),
    }
}

/// Function refs can't be compared across engines by identity — only null-ness.
fn norm(o: &Out) -> Out {
    match o {
        Out::Vals(v) => Out::Vals(
            v.iter()
                .map(|x| match x {
                    Val::FuncRef(r) => Val::FuncRef(r.map(|_| 0)),
                    Val::ExternRef(r) => Val::ExternRef(r.map(|_| 0)),
                    other => *other,
                })
                .collect(),
        ),
        t => t.clone(),
    }
}

fn wasmi_trap(e: &wasmi::Error) -> Option<Trap> {
    use wasmi::TrapCode::*;
    Some(match e.as_trap_code()? {
        UnreachableCodeReached => Trap::Unreachable,
        MemoryOutOfBounds => Trap::MemoryOutOfBounds,
        TableOutOfBounds => Trap::TableOutOfBounds,
        IndirectCallToNull => Trap::UninitializedElement,
        IntegerDivisionByZero => Trap::DivideByZero,
        IntegerOverflow => Trap::IntegerOverflow,
        BadConversionToInteger => Trap::InvalidConversion,
        StackOverflow => Trap::StackOverflow,
        BadSignature => Trap::IndirectCallType,
        OutOfFuel => Trap::OutOfFuel,
        _ => return None,
    })
}

/// Traps that differ between engines by design (resource limits, and how a bad table index is named).
fn resource(t: &Trap) -> bool {
    matches!(t, Trap::OutOfFuel | Trap::StackOverflow)
}

fn same_trap_family(a: &Trap, b: &Trap) -> bool {
    use Trap::*;
    a == b
        || matches!(
            (a, b),
            (
                UndefinedElement | UninitializedElement | TableOutOfBounds,
                UndefinedElement | UninitializedElement | TableOutOfBounds
            )
        )
}

fn args(rng: &mut Rng, ty: &wasmi::FuncType) -> Vec<Val> {
    ty.params()
        .iter()
        .map(|t| {
            let r = rng.next();
            let small = [0u64, 1, u64::MAX, 0x8000_0000, 0x7FFF_FFFF, 65535, 65536][(r % 7) as usize];
            let v = if r & 8 == 0 { small } else { rng.next() };
            match t {
                wasmi::ValType::I32 => Val::I32(v as i32),
                wasmi::ValType::I64 => Val::I64(v as i64),
                wasmi::ValType::F32 => Val::F32(v as u32),
                wasmi::ValType::F64 => Val::F64(v),
                wasmi::ValType::FuncRef => Val::FuncRef(None),
                _ => Val::ExternRef(None),
            }
        })
        .collect()
}

fn wasmtime_val(v: &Val) -> wasmtime::Val {
    match *v {
        Val::I32(x) => wasmtime::Val::I32(x),
        Val::I64(x) => wasmtime::Val::I64(x),
        Val::F32(b) => wasmtime::Val::F32(b),
        Val::F64(b) => wasmtime::Val::F64(b),
        Val::FuncRef(_) => wasmtime::Val::FuncRef(None),
        Val::ExternRef(_) => wasmtime::Val::ExternRef(None),
    }
}

fn from_wasmtime(v: &wasmtime::Val) -> Val {
    match v {
        wasmtime::Val::I32(x) => Val::I32(*x),
        wasmtime::Val::I64(x) => Val::I64(*x),
        wasmtime::Val::F32(b) => Val::F32(*b),
        wasmtime::Val::F64(b) => Val::F64(*b),
        wasmtime::Val::FuncRef(r) => Val::FuncRef(r.as_ref().map(|_| 0)),
        wasmtime::Val::ExternRef(r) => Val::ExternRef(r.as_ref().map(|_| 0)),
        _ => Val::I32(i32::MIN),
    }
}

fn wasmtime_trap(e: &wasmtime::Error) -> Option<Trap> {
    use wasmtime::Trap as T;
    Some(match e.downcast_ref::<T>()? {
        T::UnreachableCodeReached => Trap::Unreachable,
        T::MemoryOutOfBounds => Trap::MemoryOutOfBounds,
        T::TableOutOfBounds => Trap::TableOutOfBounds,
        T::IndirectCallToNull => Trap::UninitializedElement,
        T::IntegerDivisionByZero => Trap::DivideByZero,
        T::IntegerOverflow => Trap::IntegerOverflow,
        T::BadConversionToInteger => Trap::InvalidConversion,
        T::StackOverflow => Trap::StackOverflow,
        T::BadSignature => Trap::IndirectCallType,
        T::OutOfFuel => Trap::OutOfFuel,
        _ => return None,
    })
}

/// A third opinion: replays the same calls on wasmtime and returns the last one's outcome (None if it
/// couldn't tell — a resource limit, or something it reports differently).
fn wasmtime_replay(bytes: &[u8], calls: &[(String, Vec<Val>, usize)]) -> Option<Out> {
    let mut c = wasmtime::Config::new();
    c.consume_fuel(true);
    let engine = wasmtime::Engine::new(&c).ok()?;
    let module = wasmtime::Module::new(&engine, bytes).ok()?;
    let limits = wasmtime::StoreLimitsBuilder::new().memory_size(64 << 20).table_elements(100_000).build();
    let mut store = wasmtime::Store::new(&engine, limits);
    store.limiter(|l| l);
    store.set_fuel(FUEL * 4).ok()?;
    let inst = wasmtime::Instance::new(&mut store, &module, &[]).ok()?;
    let mut last = None;
    for (name, args, nres) in calls {
        let f = inst.get_func(&mut store, name)?;
        let wa: Vec<wasmtime::Val> = args.iter().map(wasmtime_val).collect();
        let mut res = vec![wasmtime::Val::I32(0); *nres];
        store.set_fuel(FUEL * 4).ok()?;
        last = Some(match f.call(&mut store, &wa, &mut res) {
            Ok(()) => Out::Vals(res.iter().map(from_wasmtime).collect()),
            Err(e) => Out::Trap(wasmtime_trap(&e)?),
        });
        if matches!(&last, Some(Out::Trap(t)) if resource(t)) {
            return None;
        }
    }
    last
}

fn agree(a: &Out, b: &Out) -> bool {
    match (norm(a), norm(b)) {
        (Out::Trap(a), Out::Trap(b)) => same_trap_family(&a, &b),
        (x, y) => x == y,
    }
}

static WASMI_WRONG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// wasmi's own panics (its translator has hit `unreachable!` on valid modules): not ours to find, so
/// counted apart, and that module is then compared JIT against interpreter only.
static WASMI_PANICKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Runs a piece of wasmi; `None` if it panicked.
fn in_wasmi<T>(f: impl FnOnce() -> T) -> Option<T> {
    let r = catch_unwind(AssertUnwindSafe(f)).ok();
    if r.is_none() {
        WASMI_PANICKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    r
}

/// Runs one module everywhere; `Err` describes a disagreement.
fn case(bytes: &[u8], rng: &mut Rng) -> Result<usize, String> {
    let (mut jit, mut int) = match (ours(bytes, true), ours(bytes, false)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(a), Err(b)) if a == b => return Ok(0),
        (a, b) => return Err(format!("instantiation differs: jit {:?} / interp {:?}", a.err(), b.err())),
    };
    let engine = {
        let mut c = wasmi::Config::default();
        c.consume_fuel(true);
        wasmi::Engine::new(&c)
    };
    let Some(wm) = in_wasmi(|| wasmi::Module::new(&engine, bytes)) else { return Ok(0) };
    let wm = wm.map_err(|e| format!("wasmi rejects a module we accept: {e}"))?;
    // The same memory and table caps as ours, so growth fails alike.
    let limits = wasmi::StoreLimitsBuilder::new().memory_size(64 << 20).table_elements(100_000).build();
    let mut store = wasmi::Store::new(&engine, limits);
    store.limiter(|l| l);
    store.set_fuel(FUEL * 4).unwrap();
    let wi =
        in_wasmi(|| wasmi::Linker::<wasmi::StoreLimits>::new(&engine).instantiate_and_start(&mut store, &wm));
    let wi = wi.and_then(Result::ok);
    let mut wasmi_ok = wi.is_some();
    let funcs: Vec<(String, wasmi::FuncType)> =
        wm.exports().filter_map(|e| e.ty().func().map(|t| (e.name().to_string(), t.clone()))).collect();
    let mut calls = 0;
    let mut log: Vec<(String, Vec<Val>, usize)> = Vec::new();
    for (name, ty) in &funcs {
        for _ in 0..2 {
            let a = args(rng, ty);
            let f = jit.module().func(name).expect("exported");
            jit.set_fuel(FUEL);
            int.set_fuel(FUEL);
            let oj = jit.call(f, &a).map_or_else(Out::Trap, Out::Vals);
            let oi = int.call(f, &a).map_or_else(Out::Trap, Out::Vals);
            calls += 1;
            log.push((name.clone(), a.clone(), ty.results().len()));
            if oj != oi {
                return Err(format!("{name}({a:?}): jit {oj:?} / interp {oi:?}"));
            }
            if jit.memory() != int.memory() {
                return Err(format!("{name}({a:?}): memory differs between jit and interpreter"));
            }
            let resource_hit = matches!(&oj, Out::Trap(t) if resource(t));
            if let (true, Some(wi)) = (wasmi_ok, &wi) {
                let wf = wi.get_func(&store, name).expect("exported");
                let mut res = vec![wasmi::Val::I32(0); ty.results().len()];
                let wa: Vec<wasmi::Val> = a.iter().map(wasmi_val).collect();
                store.set_fuel(FUEL * 4).unwrap();
                let Some(called) = in_wasmi(|| wf.call(&mut store, &wa, &mut res)) else {
                    wasmi_ok = false;
                    continue;
                };
                let ow = match called {
                    Ok(()) => Out::Vals(res.iter().map(from_wasmi).collect()),
                    Err(e) => match wasmi_trap(&e) {
                        Some(t) => Out::Trap(t),
                        None => return Err(format!("{name}: wasmi error {e}")),
                    },
                };
                let wasmi_resource = matches!(&ow, Out::Trap(t) if resource(t));
                if resource_hit || wasmi_resource {
                    // Engines count fuel and stack differently: from here their states may differ.
                    wasmi_ok = false;
                } else {
                    if !agree(&oj, &ow) {
                        // Ask wasmtime who's right.
                        match wasmtime_replay(bytes, &log) {
                            Some(ot) if agree(&oj, &ot) => {
                                WASMI_WRONG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                wasmi_ok = false;
                            }
                            ot => {
                                return Err(format!(
                                    "{name}({a:?}): tarae {oj:?} / wasmi {ow:?} / wasmtime {ot:?}"
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(calls)
}

fn mutate(rng: &mut Rng, bytes: &[u8]) -> Vec<u8> {
    let mut b = bytes.to_vec();
    for _ in 0..1 + rng.next() % 4 {
        if b.is_empty() {
            break;
        }
        let i = (rng.next() as usize) % b.len();
        match rng.next() % 4 {
            0 => b[i] = rng.next() as u8,
            1 => b[i] ^= 1 << (rng.next() % 8),
            2 => b.truncate(i),
            _ => b.insert(i, rng.next() as u8),
        }
    }
    b
}

fn main() {
    // wasmi recurses on the host stack for wasm calls; give it room.
    std::thread::Builder::new().stack_size(1 << 30).spawn(real_main).unwrap().join().unwrap();
}

fn real_main() {
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(String::as_str) == Some("wat") {
        println!("{}", wasmprinter::print_bytes(std::fs::read(&a[2]).unwrap()).unwrap());
        return;
    }
    if a.get(1).map(String::as_str) == Some("repro") {
        let bytes = std::fs::read(&a[2]).unwrap();
        let which = a[3].as_str();
        let mut rng = Rng(7);
        match which {
            "jit" | "interp" => {
                let mut i = ours(&bytes, which == "jit").unwrap();
                let names: Vec<String> = wasmi::Module::new(&wasmi::Engine::default(), &bytes[..])
                    .unwrap()
                    .exports()
                    .filter(|e| e.ty().func().is_some())
                    .map(|e| e.name().to_string())
                    .collect();
                for n in names {
                    let f = i.module().func(&n).unwrap();
                    let ty = wasmi::Module::new(&wasmi::Engine::default(), &bytes[..])
                        .unwrap()
                        .get_export(&n)
                        .unwrap()
                        .func()
                        .unwrap()
                        .clone();
                    let args = args(&mut rng, &ty);
                    i.set_fuel(FUEL);
                    println!("{n}: {:?}", i.call(f, &args));
                }
            }
            _ => println!("{:?}", case(&bytes, &mut rng)),
        }
        return;
    }
    let secs: u64 = a.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
    let mut seed: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(1);
    NO_TRAPS.store(a.get(3).is_some_and(|s| s == "notraps"), std::sync::atomic::Ordering::Relaxed);
    std::fs::create_dir_all("found").unwrap();
    std::panic::set_hook(Box::new(|_| {}));
    let start = Instant::now();
    let (mut cases, mut calls, mut mutants, mut valid_mutants, mut found) = (0u64, 0u64, 0u64, 0u64, 0u64);
    while start.elapsed() < Duration::from_secs(secs) {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let len = 4096 + (rng.next() % 16384) as usize;
        let data = rng.bytes(len);
        let module = match wasm_smith::Module::new(config(), &mut Unstructured::new(&data)) {
            Ok(m) => m.to_bytes(),
            Err(_) => {
                seed += 1;
                continue;
            }
        };
        let mut report = |kind: &str, bytes: &[u8], why: String| {
            found += 1;
            let path = format!("found/{seed}-{kind}.wasm");
            std::fs::write(&path, bytes).unwrap();
            println!("FOUND seed {seed} {kind}: {}", why.chars().take(300).collect::<String>());
        };
        std::fs::write("last.wasm", &module).unwrap();
        match catch_unwind(AssertUnwindSafe(|| case(&module, &mut rng))) {
            Ok(Ok(n)) => calls += n as u64,
            Ok(Err(why)) => report("diff", &module, why),
            Err(p) => {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()));
                report("panic", &module, msg.unwrap_or_default())
            }
        }
        cases += 1;
        // A few mutants of the same module through the decoder (and, if still valid, through everything).
        for _ in 0..4 {
            let m = mutate(&mut rng, &module);
            std::fs::write("last.wasm", &m).unwrap();
            mutants += 1;
            match catch_unwind(AssertUnwindSafe(|| {
                let ok = Module::new(&m).is_ok();
                if ok { case(&m, &mut Rng(seed | 1)).map(|_| true) } else { Ok(false) }
            })) {
                Ok(Ok(true)) => valid_mutants += 1,
                Ok(Ok(false)) => {}
                Ok(Err(why)) => {
                    // wasmi may reject what we accept for our deliberately-lenient spots; report the rest.
                    if !why.starts_with("wasmi rejects") {
                        report("mutant-diff", &m, why)
                    }
                }
                Err(_) => report("mutant-panic", &m, "panic".into()),
            }
        }
        seed += 1;
        if cases % 500 == 0 {
            println!(
                "{:>5.0}s  modules {cases}  calls {calls}  mutants {mutants} ({valid_mutants} valid)  found {found}",
                start.elapsed().as_secs_f64()
            );
        }
    }
    println!(
        "done: {:.0}s  modules {cases}  calls {calls}  mutants {mutants} ({valid_mutants} valid)  found {found}  wasmi wrong (wasmtime agrees with us) {}  wasmi panicked {}  next seed {seed}",
        start.elapsed().as_secs_f64(),
        WASMI_WRONG.load(std::sync::atomic::Ordering::Relaxed),
        WASMI_PANICKED.load(std::sync::atomic::Ordering::Relaxed)
    );
}
