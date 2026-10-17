#!/bin/sh
# The build inside the builder container; scripts/build.sh runs it.
#
#   build-in-container.sh SRC OUT release|config-only
#
# SRC is a clean copy of the repository, OUT receives bzImage, config and
# initramfs.cpio.zst. The kernel tarball is mounted at /cache/linux.tar.xz.
set -eu

src=$1
out=$2
mode=$3
dir=$(dirname "$src")
version=$(cat "$src/kernel/VERSION")

# Everything that could read the clock or the host reads these instead.
SOURCE_DATE_EPOCH=$(date -u -d "$(cat "$src/SOURCE_DATE")" +%s)
KBUILD_BUILD_TIMESTAMP=$(date -u -d "@$SOURCE_DATE_EPOCH" '+%Y-%m-%d %H:%M:%S UTC')
KBUILD_BUILD_USER=gigahost
KBUILD_BUILD_HOST=gigahost
KBUILD_BUILD_VERSION=1
export SOURCE_DATE_EPOCH KBUILD_BUILD_TIMESTAMP KBUILD_BUILD_USER KBUILD_BUILD_HOST KBUILD_BUILD_VERSION
export TZ=UTC LC_ALL=C.UTF-8

log() {
    echo "build: $*"
}

# Prints the end of a log and fails.
failed() {
    tail -n 60 "$1" >&2
    echo "build: $2" >&2
    exit 1
}

# 2. The kernel: the base, the fragments in order, olddefconfig.
log "unpacking linux-$version"
tar -C "$dir" -xf /cache/linux.tar.xz
cd "$dir/linux-$version"
cp "$src/kernel/base.config" .config
# The glob sorts in the C locale: the fragments merge in their names' order.
scripts/kconfig/merge_config.sh -m .config "$src"/kernel/fragments/*.config \
    > "$dir/merge.log" 2>&1 || failed "$dir/merge.log" "merging the fragments failed"
make olddefconfig > "$dir/olddefconfig.log" 2>&1 || failed "$dir/olddefconfig.log" "olddefconfig failed"
cp .config "$out/config"
if [ "$mode" = config-only ]; then
    exit 0
fi
if ! diff -u "$src/kernel/config" .config > "$dir/config.diff"; then
    cat "$dir/config.diff" >&2
    echo "build: kernel/config differs from the merged configuration above;" >&2
    echo "build: scripts/build.sh --update-config writes it" >&2
    exit 1
fi
log "building the kernel"
make -j16 bzImage > "$dir/kernel.log" 2>&1 || failed "$dir/kernel.log" "the kernel build failed"
cp arch/x86/boot/bzImage "$out/bzImage"

# 3. The init: static, with no build path left in the binary.
log "building the init"
export CARGO_TARGET_DIR="$dir/cargo-target"
export RUSTFLAGS="--remap-path-prefix=$src=/kernel-linux --remap-path-prefix=$CARGO_HOME=/cargo"
cd "$src"
cargo build --quiet --release --locked --target x86_64-unknown-linux-musl --manifest-path init/Cargo.toml
init=$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/gigahost-init
if readelf -lW "$init" | grep -q INTERP || readelf -dW "$init" | grep -q NEEDED; then
    echo "build: the init is not static" >&2
    exit 1
fi
if grep -q -a -F "$dir" "$init"; then
    echo "build: the init holds its build path" >&2
    exit 1
fi

# 4. The initramfs: only /init, owned by root, at the release's date.
log "packing the initramfs"
mkdir "$dir/initramfs"
cp "$init" "$dir/initramfs/init"
chmod 0755 "$dir/initramfs/init"
touch -d "@$SOURCE_DATE_EPOCH" "$dir/initramfs/init"
(cd "$dir/initramfs" && printf 'init\n' | cpio --quiet -o -H newc -R 0:0 --reproducible) \
    | zstd -q -19 -T1 > "$out/initramfs.cpio.zst"
log "done"
