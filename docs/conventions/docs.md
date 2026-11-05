# Documentation conventions

- Document every public item. The compiler enforces this.
- Document private items by default, naming their intent or what they expect: preconditions,
  invariants, ordering, or ownership.
- A doc must tell the reader something the name and signature don't. "Parses the size" fails.
  "Rejects values above `maximum` so callers can't overflow the unit multiplier" passes.
- Skip a private doc only where it would restate the code.
- Start with one sentence of about 15 words saying what the item does.
- Describe what the code does now. Decisions and alternatives belong in an ADR.
