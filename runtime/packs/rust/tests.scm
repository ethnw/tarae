; Rust tests: a function under a test attribute — #[test], #[tokio::test], #[rstest], #[test_case(…)],
; #[quickcheck], #[proptest] — maybe with other attributes and comments between. The test starts at its
; first attribute (@start), so the cursor on any of them counts.
((attribute_item
   (attribute [(identifier) @_attr (scoped_identifier name: (identifier) @_attr)])) @start
 .
 [(attribute_item) (line_comment) (block_comment)]*
 .
 (function_item name: (identifier) @name) @test
 (#any-of? @_attr "test" "rstest" "test_case" "quickcheck" "proptest"))

; …and when other attributes come before the test one (`#[ignore]` above `#[test]`)
((attribute_item) @start
 .
 [(attribute_item) (line_comment) (block_comment)]*
 .
 (attribute_item
   (attribute [(identifier) @_attr (scoped_identifier name: (identifier) @_attr)]))
 .
 [(attribute_item) (line_comment) (block_comment)]*
 .
 (function_item name: (identifier) @name) @test
 (#any-of? @_attr "test" "rstest" "test_case" "quickcheck" "proptest"))

; Modules group tests (`mod tests`), from their attributes (`#[cfg(test)]`)
(mod_item name: (identifier) @name) @group

((attribute_item) @start
 .
 [(attribute_item) (line_comment) (block_comment)]*
 .
 (mod_item name: (identifier) @name) @group)
