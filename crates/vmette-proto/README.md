# vmette-proto

Wire contracts shared across the [vmette](https://github.com/chamuka-inc/vmette)
workspace. The **leaf** crate — serde only, no workspace dependencies — so a
protocol change is a compile error everywhere at once.

It carries two wire shapes:

- **`agent`** — the guest computer-use vocabulary (`Action`, `ResponseHeader`,
  `ScrollDirection`) spoken over vsock to the in-guest desktop agent.
- **`daemon`** — the `vmetted` UNIX-socket protocol (`Request`, `Frame`,
  `DesktopRequest`, `DesktopReply`, …).

Plus the shared `geom::Rect` and `ShareMount` types. One Rust type per wire
shape, so drift between producer and consumer is caught at build time.

Part of the vmette project. MIT licensed.
