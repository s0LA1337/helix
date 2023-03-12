; Credits to: nvim-treesitter/nvim-treesitter-context
(if_expression) @context

(else_clause) @context

(match_expression) @context

(match_arm) @context

(for_expression) @context

(while_expression) @context

(loop_expression) @context
  
(closure_expression) @context

(function_item
  ("{" @context.end)
) @context

(impl_item) @context

(trait_item) @context

(struct_item) @context

(enum_item) @context

(mod_item) @context