; JUnit 4 and 5 (and TestNG's @Test): methods with a test annotation, in their classes (nested ones too)
((method_declaration
  (modifiers [(marker_annotation name: (_) @_a) (annotation name: (_) @_a)])
  name: (identifier) @name) @test
  (#match? @_a "(^|\\.)(Test|ParameterizedTest|RepeatedTest|TestFactory|TestTemplate)$"))

(class_declaration name: (identifier) @name) @group

; The file's package (`{package}` — pack.toml's vars)
(package_declaration [(identifier) (scoped_identifier)] @var.package-name)
