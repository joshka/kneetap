# Testing conventions

- Tests that go through a public surface, a library API or the binary, live in `tests/`. All other
  tests sit beside the code they prove. Prefer the second kind, because they're faster to iterate
  on.
- Keep a test linear: set up, act once, compare whole values. It should read as documentation.
- Name a test for the behavior it proves, not the function it calls.
- Assert on values rather than booleans, so a failure shows what happened.
