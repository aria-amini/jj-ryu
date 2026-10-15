# jj-ryu

Fork of `dmmulroy/jj-ryu`: a stacked-PR CLI for Jujutsu. Binary `ryu`, library `jj_ryu`.

## Check

- Run `mise run check` before a PR. It runs the PR CI checks except the MSRV build.
- `mise run e2e` pushes to and merges PRs in a real GitHub repository. Set `JJ_RYU_E2E_OWNER` and `JJ_RYU_E2E_REPO`; the repository must contain a `.jj-ryu-e2e` file. CI runs it only from the manual `E2E` workflow.

## Rules

- Keep CLI code in `src/main.rs` and `src/cli/`. Export only interface-agnostic code from `lib.rs`.
- Use `thiserror` in the library. Use `anyhow` only in `src/main.rs` and `src/cli/`.
- Do not add `.unwrap()` to production code.
- Use rustls only. native-tls breaks the musl and cross-compiled release builds.
- Do not use mockall. Its generated methods do not match the `PlatformService` return types. Use `tests/common/mock_platform.rs`.
- Native GitHub stack registration is soft-fail. A registration error must not fail `ryu submit`.
- In a stack reorder, retarget a PR off its old base before you push that base. `src/submit/plan.rs` encodes this order.

## Fork

- Sync upstream by merge. In conflicts on CI, README, or this file, keep the fork side.
- Releases go to GitHub releases only, as `X.Y.Z-fork.N`.
