## Summary

<!-- one or two sentences: what changes and why -->

## Test plan

- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` green
- [ ] (if relevant) ran on a Linux host with `/dev/fuse` access
- [ ] (if relevant) CHANGELOG.md entry added

## Risks / rollback

<!-- anything that could surprise a reviewer or break a deployment -->
