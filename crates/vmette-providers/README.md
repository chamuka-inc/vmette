# vmette-providers

The default rootfs-provider registry for
[vmette](https://github.com/chamuka-inc/vmette). Exposes `default_registry()`,
which wires the built-in providers in the single load-bearing resolution order:

```
DirProvider → Squashfs → Tar → Oci
```

The CLI and the daemon both build their registry from here, so a `--rootfs SPEC`
resolves identically whichever binary handles it. Add your own provider by
implementing `vmette::provider::RootfsProvider` in a sibling crate.

> macOS-only (depends on `vmette`). Part of the vmette project. MIT licensed.
