# Building and verifying a release

A release is four files and their checksums, in `out/<RELEASE>/`:

| File | What it is |
|---|---|
| `bzImage` | The Linux kernel, version `kernel/VERSION`, built with `kernel/config`. |
| `initramfs.cpio.zst` | A newc cpio, zstd-compressed, holding only `/init`: the static `gigahost-init` built from `init/`. |
| `config` | The kernel configuration, identical to `kernel/config`. |
| `build-info.json` | The release, the kernel tarball's URL and sha256, this repository's commit, whether the tree was dirty, the interface versions the init accepts (docs/interface.md), the builder's inputs (the sha256 of `build/Containerfile`, its pinned base image and Debian snapshot) and the Rust toolchain. |
| `SHA256SUMS` | sha256 of the four files above. |

The release name, `RELEASE`, is the kernel version, `-gh` and our build
number (`6.18.57-gh1`); a release's tag is `v<RELEASE>`.

## Build

Needs `podman` (rootless is fine), `git`, `curl` and about 3 GB of disk:

```sh
scripts/build.sh                 # out/<RELEASE>/
scripts/build.sh --out DIR       # DIR/<RELEASE>/
scripts/build.sh --check         # build twice and compare (below)
```

The build refuses a tree with uncommitted changes or untracked files, since
the release records the commit it was built from; `--allow-dirty` builds
the working tree as it is and marks `build-info.json` `"dirty": true`.

What it does:

1. Downloads the kernel tarball from `cdn.kernel.org` into
   `$XDG_CACHE_HOME/kernel-linux/` (`~/.cache/kernel-linux/`) and checks it
   against `kernel/SHA256`.
2. Builds the builder image from `build/Containerfile`, tagged
   `localhost/kernel-linux-builder:<the Containerfile's sha256>`, unless it
   exists.
3. In the builder, from a clean copy of the commit (`git archive`):
   - the kernel: `kernel/base.config`, then `kernel/fragments/*.config` in
     name order with `scripts/kconfig/merge_config.sh -m`, then
     `make olddefconfig`. The build fails, printing the difference, when
     the result differs from `kernel/config` by one line, so the
     configuration the kernel is built with is always the one in the
     repository. Then `make -j16 bzImage`.
   - the init: `cargo build --release --locked` for
     `x86_64-unknown-linux-musl`, with the source and cargo paths remapped;
     the build checks the binary is static and holds no build path.
   - the initramfs: `/init` alone, mode 0755, owned by root, dated
     `SOURCE_DATE`, packed with `cpio --reproducible` and `zstd -19`.

To change the kernel configuration, edit `kernel/base.config` or a
fragment, then run `scripts/build.sh --update-config`, which writes the
merged result to `kernel/config`; commit both.

## Pinned inputs

Every input is pinned, so the same commit builds the same files:

- the kernel tarball, by sha256 (`kernel/SHA256`);
- the builder's base image, `debian:13`, by digest (`build/Containerfile`);
- its packages (gcc, binutils, make, …), from `snapshot.debian.org` at one
  timestamp (`DEBIAN_SNAPSHOT` in `build/Containerfile`);
- `rustup-init` by version and sha256, and the Rust toolchain by version
  (`rust-toolchain.toml`), which rustup checks against its signed manifest;
- the init's crates, by `init/Cargo.lock`;
- the clock: `SOURCE_DATE` (one line, the release's date) gives
  `SOURCE_DATE_EPOCH`, `KBUILD_BUILD_TIMESTAMP` and the initramfs's dates;
  the kernel's build user, host and version are fixed
  (`gigahost`, `gigahost`, `1`). Nothing reads the host's clock, user name
  or paths.

`build-info.json` names the builder by its inputs
(`builder_containerfile_sha256`, `builder_base_image`, `debian_snapshot`),
not by the local image's ID: package installs record their own time
inside the image, so that ID differs between machines although the
compiler and every tool in it are the same. Every file of a release,
`build-info.json` included, is therefore the same on every machine.

## Reproducibility and verification

`scripts/build.sh --check` builds twice, from two copies of the commit in
two different paths, and fails unless both `SHA256SUMS` are identical.
Every release tag is built that way (`.github/workflows/ci.yml`).

To verify a release yourself, build its tag and compare the whole
`SHA256SUMS`:

```sh
git checkout v6.18.57-gh1
scripts/build.sh --out /tmp/verify
cmp /tmp/verify/6.18.57-gh1/SHA256SUMS path/to/release/SHA256SUMS
```

The files must be identical; then so are the release's files, which
`sha256sum -c SHA256SUMS` in the release's directory confirms.

## Boot tests

`tests/boot.sh [RELEASE_DIR]` boots the release under
`qemu-system-x86_64` with KVM and checks the init's behaviour: the kernel
feature checks (namespaces, cgroup v2, seccomp, overlayfs, FUSE,
WireGuard, nftables and iptables-nft, IPv6, …), exit codes and signals,
the user, working directory and environment, the hosts, resolv.conf and
host name (and that the disk's own files are untouched), refused configs
and command lines, where the output goes, the SSH keys (which replace a
symbolic link rather than write through it), the process's signal state,
and that a kernel panic or a reboot by the process ends the machine. It needs `podman`, QEMU,
`/dev/kvm`, `mkfs.ext4` and `debugfs`. Each case must end with QEMU
exiting by itself within 30 s, which is the init's power-off.

## Where the kernel source comes from

The kernel is the unmodified upstream Linux kernel, GPL-2.0-only. This
repository holds none of its source: only the version (`kernel/VERSION`),
the tarball's checksum (`kernel/SHA256`) and the configuration
(`kernel/config`, merged from `kernel/base.config` and
`kernel/fragments/`). The tarball is

```
https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-<kernel/VERSION>.tar.xz
```

which kernel.org also signs (`.tar.sign`). A release's `bzImage` is that
tarball built with `kernel/config` by `scripts/build.sh`: this repository
at the release's tag plus the named tarball are its complete corresponding
source.
