# API conventions

- Libraries hold no process-wide state. They never install a subscriber, a crypto provider, or a
  signal handler.
- Keep dependency types out of public APIs unless integration is the point.
- Library errors are opaque structs with `is_*` methods. `Display` is concise, and the cause comes
  from `source()`. Only the command uses `anyhow`.
- `Server` and `Client` are concrete types with consuming builders that validate in `build`.
- Each item has one public path. Crate roots only declare modules and re-export.
- Wire types have public fields. Everything else keeps its fields private.
- Keep key material out of types that implement `Debug`. Hold paths until it's needed.
