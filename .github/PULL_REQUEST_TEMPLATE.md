## Summary

What this change does and why.

## Related issues

Closes #

## Testing

How you verified the change. For code changes, run the same gate CI uses:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Checklist

- [ ] Builds locally and the commands above pass.
- [ ] OS-specific code is `cfg` guarded; macOS, Linux, and Windows still build.
- [ ] No secrets, tokens, pairing keys, or personal paths in the diff, logs, or fixtures.
- [ ] `CHANGELOG.md` updated under `## Unreleased` if the change is user visible.
- [ ] Docs updated (`README.md` or `docs/`) if behavior or setup changed.
