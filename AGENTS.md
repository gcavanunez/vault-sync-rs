# Vault sync

Rust daemon and CLI that syncs a notes vault through Git.

## Commands

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cargo build --release --locked
```

## Structure

- Pure logic (`model`, `scheduling`, `clock`, the render functions in `service`) has unit tests beside it.
- System effects go through `process` (Git, notifications), `lock`, `storage` and `ipc`.
- `tests/sync.rs` drives the compiled executable as a black box; `VAULT_SYNC_TEST_BINARY` points it at any build.
- `docs/architecture.md` diagrams the threads, the sync pass, scheduling and lock ownership in Mermaid. Update it in the same change when any of those change, and check that the diagrams still render.

## Safety

- Never run anything against a real vault or the user's real config (`~/.config/vault-sync/config.json`). Tests and experiments use disposable repositories under the temp directory.
- Never install, start, enable or stop the user's systemd or launchd service without explicit approval.
- Treat `config.json`, `daemon-state.json`, the lock path and the socket name and protocol as stable interfaces: installed services and daemons from other versions depend on them. Change them only with the owner's approval.
- Never add automatic merges, rebases, stashes, force-pushes or conflict resolution. Unsafe states stop the pass and report.
- Never unlink the lock file or unlock it explicitly; ownership must survive in inherited Git children.
- Every child process goes through `process::run`, so it is bounded by a timeout, killed as a group, and reaped.
- Do not commit or push without the owner's approval.
