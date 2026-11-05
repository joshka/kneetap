# Dependency conventions

- Declare dependencies in `[workspace.dependencies]` without patch versions, with default features
  off where that's practical. Crates opt into the features they use.
- A dependency that only tests need between workspace crates uses `path` alone, so that it can't
  block publishing.
- Lints live in `[workspace.lints]`. If one proves more noise than value, turn it off there with a
  comment saying why.
