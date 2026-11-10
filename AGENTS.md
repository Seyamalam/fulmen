# Rules for AI agents working on Fulmen

These rules apply to any AI coding agent. Humans follow CONTRIBUTING.md,
which says the same things in more detail.

## Before changing code

- Read docs/DESIGN.md for the area you are touching. If your change departs
  from it, update DESIGN.md in the same change, or stop and ask.
- Check docs/ROADMAP.md. Work on the current milestone unless told otherwise.
  Do not start features from later milestones unasked.

## Hard rules

- Only `crates/fulmen-py` may use `pyo3`. Core logic goes in the plain-Rust crates.
- No `unwrap`, `panic!`, `todo!`, `print!`, `dbg!` outside tests. Return errors.
- No `unsafe` unless the crate is listed in `UNSAFE_ALLOWED` in
  `scripts/guardrails.py`. Never add a crate to that list yourself; ask.
- No `#[allow]` without `reason = "..."`. Prefer `#[expect]`.
- Never hold the GIL while blocking or awaiting. Wrap blocking work in `Python::detach`.
- New dependencies go in the root `[workspace.dependencies]`. Ask before
  adding one to `fulmen-core`, `-server`, `-schema` or `-security`.
- Every new `#[pyfunction]`/`#[pyclass]` gets a typed entry in `python/fulmen/_fulmen.pyi`.
- Do not weaken a secure default, a lint, a guardrail or the coverage threshold.
- Do not create `.github/workflows`. CI/CD is local (`just ci`, `just release`).
- Never run `just release`, publish to PyPI, push tags, or rename/delete
  GitHub resources without the maintainer's explicit go-ahead.
- Never commit secrets, `.env` files or tokens.

## Done means

- `just ci` passes. If a step cannot run on your machine, say which and why.
- Hot-path changes include before/after benchmark numbers (from M1 on).
- New behaviour has tests. CHANGELOG.md `Unreleased` has an entry for user-visible changes.
- Commits use Conventional Commits with a DCO sign-off (`git commit -s`).
