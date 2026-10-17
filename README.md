# kernel-linux

The Linux kernel and the init that boot a container image as a small virtual
machine: an upstream longterm kernel built reproducibly from a configuration
kept here, and a static init that runs the image's process as it would run
under a container runtime.

## Documentation

- [docs/interface.md](docs/interface.md): the contract between the host
  that boots a container machine and the init (interface version 1): the
  kernel command line, the config, the reports and the stop command.
- [docs/building.md](docs/building.md): how a release is built
  reproducibly, how to verify one, the boot tests, and where the kernel
  source comes from.

## Layout

- `kernel/`: the kernel version, the tarball's checksum, the base
  configuration, the fragments merged on top of it, and the full result.
- `init/`: `gigahost-init`, the static init (Rust).
- `build/Containerfile`: the pinned builder.
- `scripts/build.sh`: the release build; `tests/boot.sh`: the boot tests.

## Licence

The kernel is the Linux kernel, GPL-2.0-only; this repository holds no kernel
source, only the version, its checksum and the configuration it is built
with. Everything else here is licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
