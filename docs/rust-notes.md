# Rust notes

Gotchas that cost time in this codebase. The teaching policy is in
`AGENTS.md` ("Learning Rust along the way"); this file is the accumulating
list, read when the compiler or clippy pushes back.

- Struct literals need parens in comparisons: `if x == (Foo { .. })`.
- Closure params need type annotations when there's no call context.
- `chars[cur..o]` panics if `cur > o` — sort mark offsets first.
- `#[cfg(test)]` a helper used only by tests, or clippy warns dead-code.
- `(!cond).then(|| …).flatten()` for an optional-when-condition value.
- `thiserror` tuple variants: mixing `{0}` with a bare `{}` in `#[error]` is
  "ambiguous reference to positional arguments by number" — name the extra
  arg (`expected = CONST`) instead.
- Two writers of one invariant (`sync` vs `seek`) — clamp at **every**
  write, not just the "obvious" one.
- `clippy::too_many_arguments` (7+) → group the args in a parameter struct
  (`DetectArgs`) instead of `#[allow]`-ing it.
- Pool handlers take small `Copy` accessors (`pool_sel`/`set_pool_sel`/…) so
  the borrow ends at the statement instead of borrowing all of `App` across a
  helper call.
- `let … else` for a refutable pattern that ends the function early; it keeps
  the happy path unindented and replaces a one-arm `match`.
- `Result<Option<T>, E>` for "failed" vs "not my case": `?` propagates the
  error, and the caller falls through on `None`.
- Struct-update syntax (`Foo { field, ..old.clone() }`) changes one field and
  keeps the rest — but it still moves/clones the whole struct, so it is not a
  cheap patch.
