((identifier) @variable)
(type_identifier) @type
((field_identifier) @variable.other.member)

(parameter
 name: (identifier) @variable.parameter)

(scoped_identifier
 path: (identifier) @namespace)

(
 (call_expr 
  function: (identifier) @function)
 ; (#match? @function "^[a-z]+([A-Za-z0-9_]*)+$")
)

(
 (call_expr
  function: (field_expr 
   field: (field_identifier) @function))
 ; (#match? @function "^[a-z]+([A-Za-z0-9_]*)+$")
)

(
(generic_function
 function: (identifier) @constructor)
 ; (#match? @constructor "^[A-Z]+([A-Za-z0-9_]*)+$")
)

(rune) @type

(identifier) @type (#match? @type "^[A-Z]")

(func_header name: (identifier) @function)
(struct_definition (type_identifier)) @struct

(attribute (attribute_name) @function.macro .)

(bool_lit) @constant.builtin.boolean
(int_lit) @constant.numeric.integer
(float_lit) @constant.numeric.float
(string_literal) @string
(escape_sequence) @string.escape

(line_comment) @comment

[
 "("
 ")"
 "["
 "]"
 "<"
 ">"
 "{"
 "}"
] @punctuation.bracket

[
 "."
 ","
 ";"
] @punctuation.delimiter

[
 "+"
 "-"
 "*"
 "/"
 "<"
 "<="
 ">"
 ">="
 "=="
 "==="
 "<=>"
 "!="
 "!"
 "="
 "=>"
] @operator

[
 "&"
 "&&"
 "^"
] @operator

"func" @keyword.function
"return" @keyword.return

[
 "else"
 "if"
] @keyword.conditional

[
 "while"
 "foreach"
 "in"
] @keyword.repeat

[
 "and"
 "or"
 "not"
] @keyword.control.conditional

[
 "imm"
 "mut"
 "set"
] @keyword.storage.modifier

"import" @keyword.control.import

[
 "extern"
 "exported"
 "struct"
] @keyword.storage.type


(static_array_type ["#"] @punctuation.special)
(attribute ["#" "!"] @punctuation.special)
