# Adding context queries

Helix uses tree-sitter to filter out specific scopes in which said scope may exceed the current
editor view, but which may be important for the developer to know.
These context require an accompanying tree-sitter grammar and a `context.scm` query file
to work properly.
Query files should be placed in `runtime/queries/{language}/context.scm`
when contributing to Helix. Note that to test the query files locally you should put
them under your local runtime directory (`~/.config/helix/runtime` on Linux for example).

The following [captures][tree-sitter-captures] are recognized:

| Capture Name  |
| ---           |
| `context`     |
| `context.end` |

[Example query files][context-examples] can be found in the helix GitHub repository.

## Queries for the sticky-context feature

All nodes that have a scope, should be captured with `context`, as an example a basic class.
The `context.end` is a bit more complex, this node essentially just captures the last line of 
a functions signatures (usually this should be the first block symbol, usually a `{`).
As an example, consider the following function definition:
```rs
  fn some_function(
    lhs: i32,
    rhs: i32,
  ) i32 {
  // ...
  }
```
The `context.end` node should capture the start of the function body, which has to be on the same
line as the return type of said function. In this case `{` should be matched.

[tree-sitter-captures]: https://tree-sitter.github.io/tree-sitter/using-parsers#capturing-nodes
[context-examples]: https://github.com/search?q=repo%3Ahelix-editor%2Fhelix+filename%3Acontext.scm&type=Code&ref=advsearch&l=&l=
