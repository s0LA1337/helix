(function_declaration
  (parameters) @context.params) @context

(for_statement
  body: (block)) @context

[
  ((block_label) (block))
  ((break_label) (block))
  (if_statement (block))
  (switch_expression (block))
  (switch_case (block))
  (while_expression (block))
  (struct_declaration)
  (union_declaration)
  (enum_declaration)
  (test_declaration)
  (function_declaration)
  (initializer_list)
] @context
