use super::*;

const FUEL: u64 = 1 << 40;

fn instance(wat: &str) -> Instance {
    let bytes = wat::parse_str(wat).expect("valid wat");
    let m = Module::new(&bytes).expect("module loads");
    Instance::new(m, Limits::default(), FUEL).expect("instantiates")
}

fn call(inst: &mut Instance, name: &str, args: &[Val]) -> Result<Vec<Val>, Trap> {
    let f = inst.module().func(name).expect("exported");
    inst.call(f, args)
}

fn i32s(inst: &mut Instance, name: &str, args: &[i32]) -> Result<i32, Trap> {
    let args: Vec<Val> = args.iter().map(|&a| Val::I32(a)).collect();
    match call(inst, name, &args)?.as_slice() {
        [Val::I32(v)] => Ok(*v),
        other => panic!("expected one i32, got {other:?}"),
    }
}

#[test]
fn loops_recursion_and_locals() {
    let mut i = instance(
        r#"(module
          (func $fac (export "fac") (param i64) (result i64)
            (if (result i64) (i64.eqz (local.get 0))
              (then (i64.const 1))
              (else (i64.mul (local.get 0) (call $fac (i64.sub (local.get 0) (i64.const 1)))))))
          (func (export "fib") (param i32) (result i32) (local i32 i32 i32)
            (local.set 1 (i32.const 0)) (local.set 2 (i32.const 1))
            (block $done
              (loop $next
                (br_if $done (i32.eqz (local.get 0)))
                (local.set 3 (i32.add (local.get 1) (local.get 2)))
                (local.set 1 (local.get 2))
                (local.set 2 (local.get 3))
                (local.set 0 (i32.sub (local.get 0) (i32.const 1)))
                (br $next)))
            (local.get 1)))"#,
    );
    assert_eq!(call(&mut i, "fac", &[Val::I64(20)]), Ok(vec![Val::I64(2432902008176640000)]));
    assert_eq!(i32s(&mut i, "fib", &[40]), Ok(102334155));
}

#[test]
fn multi_value_blocks_and_branch_tables() {
    let mut i = instance(
        r#"(module
          (func $swap (param i32 i32) (result i32 i32) (local.get 1) (local.get 0))
          (func (export "sub_swapped") (param i32 i32) (result i32)
            (call $swap (local.get 0) (local.get 1)) (i32.sub))
          (func (export "pick") (param i32) (result i32)
            (block $c (block $b (block $a
              (br_table $a $b $c (local.get 0)))
              (return (i32.const 10)))
              (return (i32.const 20)))
            (i32.const 30))
          (func (export "carry") (param i32) (result i32)
            ;; a block with a param, left early with extra values on the stack below
            (i32.const 7)
            (block (param i32) (result i32)
              (i32.const 100) (i32.const 200)
              (br_if 0 (local.get 0))
              (drop) (drop))
            (i32.add (i32.const 1)))
          (func (export "if_params") (param i32) (result i32)
            (i32.const 5)
            (if (param i32) (result i32) (local.get 0)
              (then (i32.const 2) (i32.mul))
              (else (i32.const 3) (i32.add)))))"#,
    );
    assert_eq!(i32s(&mut i, "sub_swapped", &[3, 10]), Ok(7));
    assert_eq!(i32s(&mut i, "pick", &[0]), Ok(10));
    assert_eq!(i32s(&mut i, "pick", &[1]), Ok(20));
    assert_eq!(i32s(&mut i, "pick", &[2]), Ok(30));
    assert_eq!(i32s(&mut i, "pick", &[99]), Ok(30), "out of range takes the default");
    assert_eq!(i32s(&mut i, "carry", &[1]), Ok(201), "br_if keeps the top value, drops the rest");
    assert_eq!(i32s(&mut i, "carry", &[0]), Ok(8));
    assert_eq!(i32s(&mut i, "if_params", &[1]), Ok(10));
    assert_eq!(i32s(&mut i, "if_params", &[0]), Ok(8));
}

#[test]
fn memory_data_and_bulk_ops() {
    let mut i = instance(
        r#"(module
          (memory (export "memory") 1 3)
          (data (i32.const 16) "hello")
          (func (export "load") (param i32) (result i32) (i32.load8_u (local.get 0)))
          (func (export "load16s") (param i32) (result i32) (i32.load16_s offset=1 (local.get 0)))
          (func (export "store") (param i32 i32) (i64.store32 (local.get 0) (i64.extend_i32_u (local.get 1))))
          (func (export "grow") (param i32) (result i32) (memory.grow (local.get 0)))
          (func (export "size") (result i32) (memory.size))
          (func (export "copy") (param i32 i32 i32) (memory.copy (local.get 0) (local.get 1) (local.get 2)))
          (func (export "fill") (param i32 i32 i32) (memory.fill (local.get 0) (local.get 1) (local.get 2))))"#,
    );
    assert_eq!(&i.memory()[16..21], b"hello");
    assert_eq!(i32s(&mut i, "load", &[17]), Ok(b'e' as i32));
    call(&mut i, "store", &[Val::I32(0), Val::I32(0xFFFF_8001u32 as i32)]).unwrap();
    assert_eq!(i32s(&mut i, "load16s", &[1]), Ok(-1), "0xFF 0xFF at 2..4, sign-extended");
    call(&mut i, "copy", &[Val::I32(18), Val::I32(16), Val::I32(5)]).unwrap();
    assert_eq!(&i.memory()[16..23], b"hehello", "overlapping copy");
    call(&mut i, "fill", &[Val::I32(16), Val::I32(b'z' as i32), Val::I32(2)]).unwrap();
    assert_eq!(&i.memory()[16..18], b"zz");
    assert_eq!(i32s(&mut i, "grow", &[1]), Ok(1));
    assert_eq!(i32s(&mut i, "grow", &[5]), Ok(-1), "past the maximum");
    assert_eq!(i32s(&mut i, "size", &[]), Ok(2));
    assert_eq!(i32s(&mut i, "load", &[2 * 65536]), Err(Trap::MemoryOutOfBounds));
    assert_eq!(i32s(&mut i, "load", &[-1]), Err(Trap::MemoryOutOfBounds), "addresses are unsigned");
    assert_eq!(
        call(&mut i, "fill", &[Val::I32(65536 * 2 - 1), Val::I32(0), Val::I32(2)]),
        Err(Trap::MemoryOutOfBounds)
    );
}

#[test]
fn indirect_calls_check_their_type() {
    let mut i = instance(
        r#"(module
          (type $bin (func (param i32 i32) (result i32)))
          (table 4 funcref)
          (elem (i32.const 0) $add $mul $neg)
          (func $add (type $bin) (i32.add (local.get 0) (local.get 1)))
          (func $mul (param i32 i32) (result i32) (i32.mul (local.get 0) (local.get 1)))
          (func $neg (param i32) (result i32) (i32.sub (i32.const 0) (local.get 0)))
          (func (export "apply") (param i32 i32 i32) (result i32)
            (call_indirect (type $bin) (local.get 1) (local.get 2) (local.get 0))))"#,
    );
    assert_eq!(i32s(&mut i, "apply", &[0, 6, 7]), Ok(13));
    assert_eq!(i32s(&mut i, "apply", &[1, 6, 7]), Ok(42), "an equal signature declared separately");
    assert_eq!(i32s(&mut i, "apply", &[2, 6, 7]), Err(Trap::IndirectCallType));
    assert_eq!(i32s(&mut i, "apply", &[3, 6, 7]), Err(Trap::UninitializedElement));
    assert_eq!(i32s(&mut i, "apply", &[9, 6, 7]), Err(Trap::UndefinedElement));
}

#[test]
fn integer_and_float_edges() {
    let mut i = instance(
        r#"(module
          (func (export "div") (param i32 i32) (result i32) (i32.div_s (local.get 0) (local.get 1)))
          (func (export "rem") (param i32 i32) (result i32) (i32.rem_s (local.get 0) (local.get 1)))
          (func (export "trunc") (param f64) (result i32) (i32.trunc_f64_s (local.get 0)))
          (func (export "trunc_sat") (param f64) (result i32) (i32.trunc_sat_f64_u (local.get 0)))
          (func (export "min") (param f32 f32) (result f32) (f32.min (local.get 0) (local.get 1)))
          (func (export "max") (param f64 f64) (result f64) (f64.max (local.get 0) (local.get 1)))
          (func (export "nearest") (param f64) (result f64) (f64.nearest (local.get 0)))
          (func (export "rotl") (param i32 i32) (result i32) (i32.rotl (local.get 0) (local.get 1)))
          (func (export "ext") (param i32) (result i32) (i32.extend8_s (local.get 0))))"#,
    );
    assert_eq!(i32s(&mut i, "div", &[7, 0]), Err(Trap::DivideByZero));
    assert_eq!(i32s(&mut i, "div", &[i32::MIN, -1]), Err(Trap::IntegerOverflow));
    assert_eq!(i32s(&mut i, "div", &[-7, 2]), Ok(-3));
    assert_eq!(i32s(&mut i, "rem", &[i32::MIN, -1]), Ok(0));
    let f64s = |i: &mut Instance, name: &str, v: f64| call(i, name, &[Val::F64(v.to_bits())]);
    assert_eq!(f64s(&mut i, "trunc", -2147483648.9), Ok(vec![Val::I32(i32::MIN)]));
    assert_eq!(f64s(&mut i, "trunc", 2147483648.0), Err(Trap::IntegerOverflow));
    assert_eq!(f64s(&mut i, "trunc", f64::NAN), Err(Trap::InvalidConversion));
    assert_eq!(f64s(&mut i, "trunc_sat", -5.0), Ok(vec![Val::I32(0)]));
    assert_eq!(f64s(&mut i, "trunc_sat", 1e20), Ok(vec![Val::I32(-1)]));
    let min = call(&mut i, "min", &[Val::F32(0.0f32.to_bits()), Val::F32((-0.0f32).to_bits())]).unwrap();
    assert_eq!(min, vec![Val::F32((-0.0f32).to_bits())], "min(+0, -0) is -0");
    let max = call(&mut i, "max", &[Val::F64(1.0f64.to_bits()), Val::F64(f64::NAN.to_bits())]).unwrap();
    assert!(matches!(max[..], [Val::F64(b)] if f64::from_bits(b).is_nan()));
    assert_eq!(f64s(&mut i, "nearest", 2.5), Ok(vec![Val::F64(2.0f64.to_bits())]), "ties to even");
    assert_eq!(i32s(&mut i, "rotl", &[0x8000_0001u32 as i32, 33]), Ok(3));
    assert_eq!(i32s(&mut i, "ext", &[0x80]), Ok(-128));
}

#[test]
fn fuel_and_depth_stop_runaway_code() {
    let mut i = instance(
        r#"(module
          (func (export "spin") (loop (br 0)))
          (func $deep (export "deep") (call $deep))
          (func (export "count") (param i32) (result i32)
            (loop (br_if 0 (local.tee 0 (i32.sub (local.get 0) (i32.const 1)))))
            (local.get 0)))"#,
    );
    i.set_fuel(1_000_000);
    assert_eq!(call(&mut i, "spin", &[]), Err(Trap::OutOfFuel));
    i.set_fuel(FUEL);
    assert_eq!(call(&mut i, "deep", &[]), Err(Trap::StackOverflow));
    // After a trap the instance still works.
    i.set_fuel(10_000);
    assert_eq!(i32s(&mut i, "count", &[100]), Ok(0));
    let used = 10_000 - i.fuel();
    assert!((100..2_000).contains(&used), "fuel tracks work done: {used}");
}

#[test]
fn refuses_what_it_does_not_support() {
    let err = |wat: &str| Module::new(&wat::parse_str(wat).unwrap()).err().map(|e| e.to_string());
    assert!(err(r#"(module (import "env" "f" (func)))"#).unwrap().contains("imports"));
    assert!(err(r#"(module (func (result i32) (i64.const 1)))"#).unwrap().contains("type mismatch"));
    assert!(err(r#"(module (func (br 1)))"#).unwrap().contains("unknown label"));
    assert!(err(r#"(module (memory 1) (func (drop (v128.load (i32.const 0)))))"#).is_some());
    assert!(
        err(r#"(module (global i32 (i32.const 0)) (func (global.set 0 (i32.const 1))))"#)
            .unwrap()
            .contains("immutable")
    );
    assert_eq!(
        Module::new(b"\0asm\x0d\0\x01\0").err().map(|e| e.to_string()).unwrap(),
        "unsupported WebAssembly version (components aren't modules)"
    );
    assert!(Module::new(b"not wasm").is_err());
    // Truncating a module anywhere never panics — mid-section it's an error.
    let good = wat::parse_str(
        r#"(module (memory 1) (func (export "f") (param i32) (result i32) (i32.load (local.get 0))))"#,
    )
    .unwrap();
    for n in 0..good.len() {
        let _ = Module::new(&good[..n]);
    }
    assert!(Module::new(&good[..good.len() - 1]).is_err());
}

#[test]
fn start_function_and_globals() {
    let mut i = instance(
        r#"(module
          (global $base i32 (i32.const 40))
          (global $g (mut i32) (global.get $base))
          (func $init (global.set $g (i32.add (global.get $g) (i32.const 2))))
          (start $init)
          (func (export "get") (result i32) (global.get $g)))"#,
    );
    assert_eq!(i32s(&mut i, "get", &[]), Ok(42));
}

#[test]
fn the_jit_runs_where_there_is_one() {
    let i = instance(r#"(module (func (export "f") (result i32) (i32.const 1)))"#);
    assert_eq!(i.module().jit.is_some(), cfg!(any(target_arch = "aarch64", target_arch = "x86_64")));
}

/// The same calls through the JIT and the interpreter agree, traps included.
#[test]
fn jit_and_interpreter_agree() {
    let wat = r#"(module
      (memory 1)
      (func $fib (export "fib") (param i32) (result i32)
        (if (result i32) (i32.lt_u (local.get 0) (i32.const 2))
          (then (local.get 0))
          (else (i32.add (call $fib (i32.sub (local.get 0) (i32.const 1)))
                         (call $fib (i32.sub (local.get 0) (i32.const 2)))))))
      (func (export "mem") (param i32) (result i32)
        (i32.store (local.get 0) (i32.const 0x01020304))
        (i32.load8_u offset=1 (local.get 0))))"#;
    let bytes = wat::parse_str(wat).unwrap();
    for (name, arg) in [("fib", 20), ("mem", 100), ("mem", 65535), ("mem", -1)] {
        let mut outs = Vec::new();
        for jit in [true, false] {
            let m = Module::new(&bytes).unwrap();
            let mut i = Instance::new(m, Limits { jit, ..Limits::default() }, FUEL).unwrap();
            outs.push(i32s(&mut i, name, &[arg]));
        }
        assert_eq!(outs[0], outs[1], "{name}({arg})");
    }
}

/// Found by the fuzzer: reference-typed locals start out null.
#[test]
fn reference_locals_start_null() {
    let mut i = instance(
        r#"(module (func (export "f") (result i32) (local funcref externref)
             (i32.add (ref.is_null (local.get 0)) (ref.is_null (local.get 1)))))"#,
    );
    assert_eq!(i32s(&mut i, "f", &[]), Ok(2));
}

/// Found by the fuzzer (x86-64): a branch table jumping back to a loop head.
#[test]
fn branch_table_back_to_a_loop() {
    let mut i = instance(
        r#"(module (func (export "f") (param i32) (result i32) (local i32)
             (loop $top
               (local.set 1 (i32.add (local.get 1) (i32.const 1)))
               (local.set 0 (i32.sub (local.get 0) (i32.const 1)))
               (block $out (br_table $top $out (i32.eqz (local.get 0)))))
             (local.get 1)))"#,
    );
    assert_eq!(i32s(&mut i, "f", &[5]), Ok(5));
}

/// Found on Linux x86-64: rounding a signaling NaN gives a quiet one (libm's `floorf` returns it unchanged).
#[test]
fn rounding_a_signaling_nan_quiets_it() {
    for jit in [true, false] {
        let bytes = wat::parse_str(
            r#"(module (func (export "f") (result i32) (i32.reinterpret_f32 (f32.floor (f32.const -nan:0x200000)))))"#,
        )
        .unwrap();
        let m = Module::new(&bytes).unwrap();
        let mut i = Instance::new(m, Limits { jit, ..Limits::default() }, FUEL).unwrap();
        let bits = i32s(&mut i, "f", &[]).unwrap() as u32;
        assert_eq!(bits & 0x7FC0_0000, 0x7FC0_0000, "quiet NaN (jit {jit}): {bits:#x}");
    }
}

#[test]
fn bulk_ops_pay_fuel_per_byte() {
    let wat = r#"(module (memory 16) (table 4 funcref)
        ;; a loop of 1 MB fills: a few instructions a turn, but a megabyte of work
        (func (export "fills") (loop (memory.fill (i32.const 0) (i32.const 1) (i32.const 1048576)) (br 0)))
        (func (export "once") (param i32)
          (memory.fill (i32.const 0) (i32.const 7) (local.get 0))
          (memory.copy (i32.const 100) (i32.const 0) (local.get 0))
          (table.fill (i32.const 0) (ref.null func) (i32.const 3))
          (drop (memory.grow (i32.const 1))))
        (func (export "out_of_bounds") (memory.fill (i32.const 0) (i32.const 0) (i32.const -1))))"#;
    let bytes = wat::parse_str(wat).unwrap();
    let mut left = Vec::new();
    for jit in [true, false] {
        let fresh = |fuel| {
            Instance::new(Module::new(&bytes).unwrap(), Limits { jit, ..Limits::default() }, fuel).unwrap()
        };
        let start = std::time::Instant::now();
        assert_eq!(call(&mut fresh(1 << 30), "fills", &[]), Err(Trap::OutOfFuel));
        assert!(start.elapsed() < std::time::Duration::from_secs(5), "stopped after about a thousand fills");
        let mut i = fresh(1 << 20);
        call(&mut i, "once", &[Val::I32(1000)]).unwrap();
        // 1000 filled + 1000 copied + 3 table elements + a 64 KiB page, and a few instructions
        let spent = (1 << 20) - i.fuel();
        assert!((2000 + 3 + 65536..2000 + 3 + 65536 + 20).contains(&spent), "{spent}");
        left.push(i.fuel());
        // Work that traps for bounds doesn't count as done
        let mut i = fresh(100);
        assert_eq!(call(&mut i, "out_of_bounds", &[]), Err(Trap::MemoryOutOfBounds));
        // Not enough fuel for the fill: it doesn't happen
        let mut i = fresh(500);
        assert_eq!(call(&mut i, "once", &[Val::I32(1000)]), Err(Trap::OutOfFuel));
        assert_eq!(i.memory()[0], 0);
    }
    assert_eq!(left[0], left[1], "the JIT and the interpreter charge the same");
}

/// Float locals live in float registers (homes, and the cache's float side): they must come through calls
/// (which clobber the caller-saved ones), helpers (which read the frame), moves between the integer and
/// float sides, and traps — with the same results as the interpreter.
#[test]
fn float_registers_come_through_calls_helpers_and_moves() {
    let wat = r#"(module
      (memory 1)
      ;; a loop over enough float locals to take every float home register — clobbering the caller's
      ;; caller-saved ones
      (func $bump (param f64) (result f64)
        (local f64 f64 f64 f64 f64 f64) (local $k i32)
        (loop $again
          (local.set 1 (f64.mul (local.get 0) (f64.const 1.5)))
          (local.set 2 (f64.sqrt (f64.add (f64.abs (local.get 1)) (f64.const 2))))
          (local.set 3 (f64.add (local.get 3) (local.get 2)))
          (local.set 4 (f64.sub (local.get 4) (local.get 3)))
          (local.set 5 (f64.add (local.get 5) (local.get 4)))
          (local.set 6 (f64.mul (local.get 5) (f64.const 0.25)))
          (local.set 0 (f64.add (local.get 0) (local.get 6)))
          (br_if $again (i32.lt_u (local.tee $k (i32.add (local.get $k) (i32.const 1))) (i32.const 3))))
        (f64.sub (local.get 1) (local.get 2)))
      (func (export "run") (param i32) (result i64)
        (local $a f64) (local $b f64) (local $c f32) (local $bits i64) (local $i i32)
        ;; more float locals than callee-saved float registers: some homes are caller-saved
        (local $d f64) (local $e f64) (local $f f64) (local $g f64)
        (local.set $a (f64.const 1.25))
        (local.set $b (f64.const -0.5))
        (local.set $c (f32.const 3))
        (loop $next
          ;; a call in the loop, with float homes live across it
          (local.set $d (f64.add (local.get $d) (f64.const 1)))
          (local.set $e (f64.mul (local.get $d) (f64.const 0.5)))
          (local.set $f (f64.sub (local.get $e) (local.get $f)))
          (local.set $g (f64.add (local.get $g) (local.get $f)))
          (local.set $a (f64.add (local.get $a) (call $bump (local.get $b))))
          (local.set $g (f64.add (local.get $g) (f64.mul (local.get $d) (local.get $e))))
          ;; float → bits → float, through memory and a helper (memory.fill reads the frame)
          (f64.store (i32.const 8) (local.get $a))
          (memory.fill (i32.const 64) (i32.const 1) (i32.const 16))
          (local.set $b (f64.div (f64.load (i32.const 8)) (f64.const 3)))
          (local.set $bits (i64.add (local.get $bits) (i64.reinterpret_f64 (local.get $b))))
          (local.set $c (f32.demote_f64 (f64.promote_f32 (f32.add (local.get $c) (f32.const 0.25)))))
          (local.set $a (select (local.get $a) (local.get $b) (i32.and (local.get $i) (i32.const 1))))
          (br_if $next (i32.lt_u (local.tee $i (i32.add (local.get $i) (i32.const 1))) (local.get 0))))
        ;; a trap with float homes in use, when asked
        (if (i32.eq (local.get 0) (i32.const 7)) (then (unreachable)))
        (i64.add (i64.add (local.get $bits) (i64.reinterpret_f64 (f64.add (local.get $g) (local.get $f))))
          (i64.add (i64.reinterpret_f64 (local.get $a)) (i64.extend_i32_u (i32.reinterpret_f32 (local.get $c)))))))"#;
    let bytes = wat::parse_str(wat).unwrap();
    for n in [1, 2, 7, 50] {
        let mut outs = Vec::new();
        for jit in [true, false] {
            let m = Module::new(&bytes).unwrap();
            let mut i = Instance::new(m, Limits { jit, ..Limits::default() }, FUEL).unwrap();
            // After a trap the instance still runs, and the caller's own float registers are intact
            let before: f64 = std::hint::black_box(0.1) * 3.0;
            outs.push(call(&mut i, "run", &[Val::I32(n)]));
            assert_eq!(before, std::hint::black_box(0.1) * 3.0);
            outs.push(call(&mut i, "run", &[Val::I32(3)]));
        }
        assert_eq!(outs[..2], outs[2..], "run({n})");
    }
}

/// A function reference from the host must name one of the module's functions (the JIT's `call_indirect`
/// trusts what's in tables).
#[test]
fn foreign_function_references_are_refused() {
    let wat = r#"(module (table 1 funcref) (type $t (func))
        (func (export "put") (param funcref) (table.set (i32.const 0) (local.get 0)) (call_indirect (type $t) (i32.const 0))))"#;
    for jit in [true, false] {
        let m = Module::new(&wat::parse_str(wat).unwrap()).unwrap();
        let mut i = Instance::new(m, Limits { jit, ..Limits::default() }, FUEL).unwrap();
        assert_eq!(call(&mut i, "put", &[Val::FuncRef(Some(1000))]), Err(Trap::BadCall));
        assert_eq!(call(&mut i, "put", &[Val::FuncRef(None)]), Err(Trap::UninitializedElement));
    }
}

/// A function too big for AArch64's conditional branches (±1 MB) to reach the traps after it: the module
/// runs on the interpreter rather than branch somewhere else (found by review).
#[test]
fn a_function_too_big_for_its_branches_is_interpreted() {
    // An early out-of-bounds load, then 60 000 more, each needing its own bounds check
    let loads: String =
        (0..60_000).map(|i| format!("(drop (i32.load offset={} (local.get 0)))", i * 4)).collect();
    let wat =
        format!(r#"(module (memory 2) (func (export "f") (param i32) (result i32) {loads} (i32.const 7)))"#);
    let m = Module::new(&wat::parse_str(&wat).unwrap()).unwrap();
    if cfg!(target_arch = "aarch64") {
        assert!(!m.compiled(), "branches from its start can't reach its end");
    }
    let mut i = Instance::new(m, Limits::default(), FUEL).unwrap();
    assert_eq!(call(&mut i, "f", &[Val::I32(1 << 20)]), Err(Trap::MemoryOutOfBounds));
    assert_eq!(i32s(&mut i, "f", &[0]), Err(Trap::MemoryOutOfBounds), "offsets reach past 2 pages");
}

/// Values carried by the thousand go through loops, not one copy each: a small module can't compile to
/// a huge one (found by review — a 21 KB module made 840 MB of x86-64 code), and copies stay right.
#[test]
fn many_values_dont_make_much_code() {
    let n = 1000; // the most results a type may have
    let results = "i32 ".repeat(n);
    let consts: String = (0..n).map(|i| format!("(i32.const {i})")).collect();
    let returns = "(return) ".repeat(200);
    // Dead `return`s after the results, and a br_table carrying them all
    let wat = format!(
        r#"(module
          (func $many (result {results}) {consts} (return) {returns})
          (func $table (param i32) (result {results})
            (block $a (result {results}) (block $b (result {results}) {consts} (local.get 0) (br_table $a $b $a $b $a)) (return)))
          (func (export "sum") (param i32) (result i32) (local $s i32)
            {sum_many}
            (i32.add (local.get $s) (i32.const 0))))"#,
        sum_many = "(call $many) ".to_string()
            + &"(local.get $s) (i32.add) (local.set $s) ".repeat(n)
            + "(call $table (local.get 0)) "
            + &"(drop) ".repeat(n)
    );
    let bytes = wat::parse_str(&wat).unwrap();
    let m = Module::new(&bytes).unwrap();
    assert!(m.compiled());
    let mut outs = Vec::new();
    for jit in [true, false] {
        let m = Module::new(&bytes).unwrap();
        let mut i = Instance::new(m, Limits { jit, ..Limits::default() }, FUEL).unwrap();
        outs.push(i32s(&mut i, "sum", &[1]));
    }
    assert_eq!(outs[0], outs[1]);
    assert_eq!(outs[0], Ok((0..n as i32).sum()));
}

/// Fuel pays for work, not ops: a loop carrying 1000 values, or calling a function with 20 000 locals to
/// zero, pays for them each turn (found by review: one fuel an op let such loops run for hours).
#[test]
fn fuel_pays_for_moves_and_zeroed_locals() {
    let vals = "(i32.const 0) ".repeat(1000);
    let params = "i32 ".repeat(1000);
    let wat = format!(
        r#"(module (global $turns (export "turns") (mut i32) (i32.const 0))
          (type $carry (func (param {params})))
          (func (export "carry") {vals}
            (loop $l (type $carry)
              (global.set $turns (i32.add (global.get $turns) (i32.const 1)))
              ;; one more value: the thousand carried shift down a slot each turn
              (i32.const 0)
              (br $l)))
          (func $big (local {locals}))
          (func (export "calls") (loop $l (call $big)
            (global.set $turns (i32.add (global.get $turns) (i32.const 1))) (br $l))))"#,
        locals = "i64 ".repeat(20_000)
    );
    let bytes = wat::parse_str(&wat).unwrap();
    for jit in [true, false] {
        for (f, per_turn) in [("carry", 1000), ("calls", 20_000)] {
            let m = Module::new(&bytes).unwrap();
            let mut i = Instance::new(m, Limits { jit, ..Limits::default() }, 1_000_000).unwrap();
            assert_eq!(call(&mut i, f, &[]), Err(Trap::OutOfFuel));
            let Some(Val::I32(turns)) = i.global("turns") else { panic!() };
            assert!(turns <= 1_000_000 / per_turn, "{f}: {turns} turns on a million fuel");
        }
    }
}

/// Loading is bounded too: locals across a module, params and results per type, tables, and the steps
/// compiling may take (found by review: small modules took gigabytes, or ten seconds, to load).
#[test]
fn loading_is_bounded() {
    let refused = |wat: String| Module::new(&wat::parse_str(wat).unwrap()).err().map(|e| e.to_string());
    // 100 functions of 50 000 locals each — a 900-byte module, built directly
    let leb = |mut n: u32, out: &mut Vec<u8>| loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        out.push(if n == 0 { b } else { b | 0x80 });
        if n == 0 {
            break;
        }
    };
    let mut body = vec![1];
    leb(50_000, &mut body);
    body.extend([0x7E, 0x0B]);
    let mut code = Vec::new();
    leb(100, &mut code);
    for _ in 0..100 {
        leb(body.len() as u32, &mut code);
        code.extend(&body);
    }
    let mut m = b"\0asm\x01\0\0\0\x01\x04\x01\x60\0\0".to_vec();
    let mut funcs = Vec::new();
    leb(100, &mut funcs);
    funcs.extend([0; 100]);
    for (id, sec) in [(3u8, funcs), (10, code)] {
        m.push(id);
        leb(sec.len() as u32, &mut m);
        m.extend(sec);
    }
    let e = Module::new(&m).err().map(|e| e.to_string());
    assert!(e.as_deref().is_some_and(|e| e.contains("too many locals in the module")), "{e:?}");
    assert!(
        refused(format!("(module (type (func (param {}))))", "i32 ".repeat(1001)))
            .is_some_and(|e| e.contains("too many params"))
    );
    assert!(
        refused(format!("(module {})", "(table 1 funcref) ".repeat(101)))
            .is_some_and(|e| e.contains("too many tables"))
    );
    // A deep operand stack and a local written again and again: each write scans the stack — refused by its
    // work's count (`MAX_WORK`), never by the clock
    let deep = format!(
        "(module (func (local i32 i32) {} {} {}))",
        "(i32.const 0) ".repeat(60_000),
        "(local.set 0 (local.get 1)) ".repeat(20_000),
        "(drop) ".repeat(60_000)
    );
    assert!(refused(deep).is_some_and(|e| e.contains("too complex")));
}

/// Bounds at the edge of memory, with the fast check (address ≤ length − 4095) failing and the exact one
/// deciding: accesses ending exactly at the end pass, one byte more traps — in a one-page memory, a grown
/// one, an empty one, and with offsets past the fast check's reach.
#[test]
fn bounds_at_the_edge_of_memory() {
    let wat = r#"(module (memory (export "m") 1)
      (func (export "load") (param i32) (result i32) (i32.load (local.get 0)))
      (func (export "load8_far") (param i32) (result i32) (i32.load8_u offset=70000 (local.get 0)))
      (func (export "store") (param i32) (i64.store (local.get 0) (i64.const -1)))
      (func (export "grow") (result i32) (memory.grow (i32.const 1)))
      (func (export "size") (result i32) (memory.size)))"#;
    let bytes = wat::parse_str(wat).unwrap();
    for jit in [true, false] {
        let mut i =
            Instance::new(Module::new(&bytes).unwrap(), Limits { jit, ..Limits::default() }, FUEL).unwrap();
        let page = 65536;
        assert_eq!(i32s(&mut i, "load", &[page - 4]), Ok(0), "ends at the end");
        assert_eq!(i32s(&mut i, "load", &[page - 3]), Err(Trap::MemoryOutOfBounds));
        assert_eq!(i32s(&mut i, "load", &[page - 4096]), Ok(0), "just under the fast check's limit");
        assert_eq!(i32s(&mut i, "load", &[-1]), Err(Trap::MemoryOutOfBounds));
        assert_eq!(call(&mut i, "store", &[Val::I32(page - 8)]), Ok(vec![]));
        assert_eq!(call(&mut i, "store", &[Val::I32(page - 7)]), Err(Trap::MemoryOutOfBounds));
        assert_eq!(i32s(&mut i, "load8_far", &[0]), Err(Trap::MemoryOutOfBounds), "offset past one page");
        assert_eq!(i32s(&mut i, "grow", &[]), Ok(1));
        assert_eq!(i32s(&mut i, "size", &[]), Ok(2));
        assert_eq!(
            i32s(&mut i, "load", &[page - 3]),
            Ok(0x00FF_FFFF),
            "in bounds once grown (3 bytes of the -1 stored)"
        );
        assert_eq!(i32s(&mut i, "load8_far", &[0]), Ok(0));
        assert_eq!(i32s(&mut i, "load", &[2 * page - 3]), Err(Trap::MemoryOutOfBounds));
    }
    // No memory pages at all: the limit is negative, everything goes to the exact check and traps
    let empty = wat::parse_str(
        r#"(module (memory 0) (func (export "load") (param i32) (result i32) (i32.load (local.get 0))))"#,
    )
    .unwrap();
    for jit in [true, false] {
        let mut i =
            Instance::new(Module::new(&empty).unwrap(), Limits { jit, ..Limits::default() }, FUEL).unwrap();
        assert_eq!(i32s(&mut i, "load", &[0]), Err(Trap::MemoryOutOfBounds));
    }
}
