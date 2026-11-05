# Code conventions

- Order items for reading: a type with its impls, callers before callees, and public before private
  when those don't decide it. These are defaults; choose the order that reduces jumping around.
- Use `foo.rs` with a `foo/` directory, never `mod.rs`. Clippy enforces this.
- Group imports by module, which rustfmt enforces. Use a qualified path when it reads better.
- Suppress a lint with `#[expect(lint, reason = "...")]`, never `#[allow]`, except on generated code.
- Keep tool configuration in `.config/` where the tool supports it. `rustfmt.toml` stays at the root
  because rustfmt can't find it elsewhere, and `cargo-deny` needs an explicit `--config`.
