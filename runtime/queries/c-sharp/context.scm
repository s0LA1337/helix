; Credits to nvim-treesitter-context

(interface_declaration
  body: (_) @context.params
) @context

(class_declaration
  body: (_) @context.params
) @context

(enum_declaration
  body: (_) @context.params
) @context

(struct_declaration
  body: (_) @context.params
) @context

(record_declaration
  body: (_) @context.params
) @context

(namespace_declaration
  body: (_) @context.params
) @context

(constructor_declaration
  body: (_) @context.params
) @context

(destructor_declaration
  body: (_) @context.params
) @context

(method_declaration
  body: (_) @context.params
) @context

(switch_statement
  body: (_) @context.params
) @context

(for_statement
  body: (_) @context.params
) @context

(if_statement
  consequence: (_) @context.params
) @context

([
  (do_statement)
  (while_statement)
] @context)

(try_statement
  body: (_) @context.params
) @context

(catch_clause
  body: (_) @context.params
) @context

(finally_clause) @context
