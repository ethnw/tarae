; Python tests: `test…` functions and methods (from their decorators, when they have some). Classes
; group them the way pytest collects them: `Test…`, or a unittest `TestCase` of any name (with their
; decorators, too).
((function_definition name: (identifier) @name) @test
  (#match? @name "^test"))

((decorated_definition definition: (function_definition name: (identifier) @name)) @test
  (#match? @name "^test"))

((class_definition name: (identifier) @name) @group
  (#match? @name "^Test"))

((class_definition name: (identifier) @name superclasses: (argument_list) @_bases) @group
  (#match? @_bases "TestCase"))

((decorated_definition definition: (class_definition name: (identifier) @name)) @group
  (#match? @name "^Test"))

((decorated_definition
   definition: (class_definition name: (identifier) @name superclasses: (argument_list) @_bases)) @group
  (#match? @_bases "TestCase"))
