# gpam-core

The shared core of [gpam](https://github.com/michalskalski/gpam), a client for [Google Cloud Privileged Access Manager](https://docs.cloud.google.com/iam/docs/pam-overview). It is the library behind the two frontends:

- [`gpam`](https://crates.io/crates/gpam), the terminal UI
- [`gpam-gui`](https://crates.io/crates/gpam-gui), the desktop UI

If you want to use gpam, install one of those. This crate is published because they depend on it.

## What is in it

- The Privileged Access Manager and Resource Manager backends, plus a demo backend with sample data
- The local SQLite cache of entitlements, scopes and tracked grants
- Background refresh and grant polling
- The local socket that receives approval events
- The view state that the frontends render: search, filters, marks, the request flow and the approval flow

## Stability

The API follows the needs of the two frontends and can change in any release. Each frontend release depends on one exact version of this crate.

## License

Licensed under either of [MIT](https://github.com/michalskalski/gpam/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/michalskalski/gpam/blob/main/LICENSE-APACHE) at your option.
