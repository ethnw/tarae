; Go tests (go test's rule: the prefix, then anything but a lowercase letter).
; `TestXxx` — also in a run of the file's tests.
((function_declaration name: (identifier) @name) @test
  (#match? @name "^Test([^a-z]|$)"))

; Benchmarks — run on their own, with -bench.
((function_declaration name: (identifier) @name) @test
  (#match? @name "^Benchmark([^a-z]|$)")
  (#set! kind "bench"))

; Fuzz tests and examples — run on their own.
((function_declaration name: (identifier) @name) @test
  (#match? @name "^(Fuzz|Example)([^a-z]|$)")
  (#set! kind "single"))
