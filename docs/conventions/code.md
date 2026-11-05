# Code conventions

- Order items for reading: a type with its impls, callers before callees, and public before private
  when those don't decide it. These are defaults; choose the order that reduces jumping around.
- Use `foo.rs` with a `foo/` directory, never `mod.rs`. Clippy enforces this.
- Group imports by module, which rustfmt enforces. Use a qualified path when it reads better.
- Name log events `<component>.<operation>.<outcome>` and put the details in fields. Libraries emit
  events but never install a subscriber.
- Suppress a lint with `#[expect(lint, reason = "...")]`, never `#[allow]`, except on generated code.
- Don't take `bool` parameters, which read as a bare `true` at the call site. Split the function, or
  pass the value the decision comes from. Clippy enforces this.
- Keep tool configuration in `.config/` where the tool supports it. `rustfmt.toml` and `clippy.toml`
  stay at the root because those tools can't find them elsewhere without help, and `cargo-deny`
  needs an explicit `--config`.
- Put a blank line before a doc comment, including on struct fields and enum variants, so long lists
  stay easy to scan.
