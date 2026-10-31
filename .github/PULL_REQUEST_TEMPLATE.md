## Summary

<!-- What does this PR do, and why? -->

Closes #

## Changes

-

## Testing

<!-- How did you verify it? Paste relevant serial output. -->

- [ ] `cargo xtask build` succeeds
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] `cargo xtask test` passes
- [ ] Boots in QEMU (`cargo xtask run`)

## Checklist

- [ ] Commits follow Conventional Commits (`type(scope): summary`), **no attribution/co-author trailers**
- [ ] `unsafe` only in `hal`, each block with a `// SAFETY:` comment
- [ ] Docs in `docs/` updated (or not needed)
- [ ] Threat model updated (if security-relevant)
- [ ] ADR added for significant design decisions (or not needed)
- [ ] `CHANGELOG.md` updated under *Unreleased*
- [ ] `docs/roadmap.md` updated if this completes a roadmap item or milestone
