#!/bin/sh
# Builds a release: the kernel, the init and its initramfs, in the builder
# container (build/Containerfile), from a clean copy of the committed tree.
#
#   scripts/build.sh [--check] [--out DIR] [--allow-dirty]
#   scripts/build.sh --update-config
#
# --check          build twice, from two different source paths, and fail
#                  unless both SHA256SUMS are identical
# --out DIR        where out/<RELEASE>/ goes (default: out/ in the tree)
# --allow-dirty    build the working tree as it is (uncommitted changes and
#                  untracked files included); build-info.json says "dirty"
# --update-config  merge the base and the fragments and write the result to
#                  kernel/config, without building (implies --allow-dirty)
#
# docs/building.md describes the inputs and how to verify a release.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
check=false
allow_dirty=false
update_config=false
out_root=$repo/out

die() {
    echo "build.sh: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case $1 in
        --check) check=true ;;
        --allow-dirty) allow_dirty=true ;;
        --update-config) update_config=true; allow_dirty=true ;;
        --out)
            [ $# -ge 2 ] || die "--out needs a directory"
            out_root=$2
            shift
            ;;
        -h|--help)
            sed -n '2,17p' "$0"
            exit 0
            ;;
        *) die "unknown argument: $1" ;;
    esac
    shift
done

for tool in podman git curl sha256sum tar; do
    command -v "$tool" > /dev/null || die "$tool is not installed"
done

release=$(cat "$repo/RELEASE")
version=$(cat "$repo/kernel/VERSION")
tarball_sha256=$(cat "$repo/kernel/SHA256")
major=${version%%.*}
tarball_url=https://cdn.kernel.org/pub/linux/kernel/v$major.x/linux-$version.tar.xz

dirty=false
if [ -n "$(git -C "$repo" status --porcelain --untracked-files=all)" ]; then
    $allow_dirty || die "the tree has uncommitted changes or untracked files (--allow-dirty builds it anyway)"
    dirty=true
fi
commit=$(git -C "$repo" rev-parse HEAD)

# 1. The kernel source, cached and checked.
cache=${XDG_CACHE_HOME:-$HOME/.cache}/kernel-linux
tarball=$cache/linux-$version.tar.xz
mkdir -p "$cache"
if [ ! -f "$tarball" ]; then
    echo "build.sh: downloading $tarball_url"
    curl -fsSL --retry 5 -o "$tarball.part" "$tarball_url"
    mv "$tarball.part" "$tarball"
fi
echo "$tarball_sha256  $tarball" | sha256sum -c --quiet - \
    || die "$tarball does not match kernel/SHA256 (delete it to download it again)"

# The builder, tagged by its Containerfile's sha256.
tag=$(sha256sum "$repo/build/Containerfile" | cut -c1-64)
image=localhost/kernel-linux-builder:$tag
if ! podman image exists "$image"; then
    echo "build.sh: building $image"
    podman build -q -t "$image" -f "$repo/build/Containerfile" "$repo/build" > /dev/null
fi
image_id=$(podman image inspect --format '{{.Id}}' "$image")
snapshot=$(sed -n 's/^ARG DEBIAN_SNAPSHOT=//p' "$repo/build/Containerfile")
toolchain=$(sed -n 's/^channel = "\(.*\)"$/\1/p' "$repo/rust-toolchain.toml")
interfaces=$(sed -n 's/^pub const VERSIONS: &\[u32\] = &\[\(.*\)\];$/\1/p' "$repo/init/src/cmdline.rs")
if [ -z "$snapshot" ] || [ -z "$toolchain" ] || [ -z "$interfaces" ]; then
    die "cannot read the snapshot, toolchain or interface versions"
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/kernel-linux-build.XXXXXX")
cleanup() {
    # The container's files belong to subordinate IDs only on odd setups;
    # podman unshare removes those too.
    rm -rf "$work" 2> /dev/null || podman unshare rm -rf "$work"
}
trap cleanup EXIT INT TERM

# Copies the tree to build into $1: the commit, or with --allow-dirty the
# working tree's tracked and untracked (not ignored) files.
copy_tree() {
    mkdir -p "$1"
    if $dirty; then
        (cd "$repo" && git ls-files --cached --others --exclude-standard \
            | while IFS= read -r file; do
                if [ -e "$file" ]; then printf '%s\n' "$file"; fi
            done) \
            | tar -C "$repo" -cf - -T - | tar -C "$1" -xf -
    else
        git -C "$repo" archive --format=tar HEAD | tar -C "$1" -xf -
    fi
}

# Builds once under $work/$1 (so each build has its own source path) and
# leaves the release files in $work/$1/out.
build_once() {
    dir=$work/$1
    copy_tree "$dir/src"
    mkdir -p "$dir/out"
    podman run --rm --security-opt label=disable \
        -v "$dir:$dir" -v "$tarball:/cache/linux.tar.xz:ro" -w "$dir" \
        "$image" sh "$dir/src/scripts/build-in-container.sh" "$dir/src" "$dir/out" "$2"
}

if $update_config; then
    build_once config config-only
    cp "$work/config/out/config" "$repo/kernel/config"
    echo "build.sh: wrote kernel/config"
    exit 0
fi

write_info() {
    cat > "$1/build-info.json" <<JSON
{
  "release": "$release",
  "kernel_version": "$version",
  "kernel_tarball_url": "$tarball_url",
  "kernel_tarball_sha256": "$tarball_sha256",
  "repo_commit": "$commit",
  "dirty": $dirty,
  "interface_versions": [$interfaces],
  "builder_image_digest": "$image_id",
  "debian_snapshot": "$snapshot",
  "rust_toolchain": "$toolchain"
}
JSON
    (cd "$1" && sha256sum bzImage build-info.json config initramfs.cpio.zst > SHA256SUMS)
}

build_once a release
write_info "$work/a/out"
if $check; then
    build_once second-build release
    write_info "$work/second-build/out"
    if ! cmp -s "$work/a/out/SHA256SUMS" "$work/second-build/out/SHA256SUMS"; then
        diff "$work/a/out/SHA256SUMS" "$work/second-build/out/SHA256SUMS" >&2 || true
        die "the two builds differ: the build is not reproducible"
    fi
    echo "build.sh: two builds from different paths are identical"
fi

target=$out_root/$release
rm -rf "$target"
mkdir -p "$target"
cp "$work/a/out/bzImage" "$work/a/out/initramfs.cpio.zst" "$work/a/out/config" \
    "$work/a/out/build-info.json" "$work/a/out/SHA256SUMS" "$target/"
echo "build.sh: $target"
cat "$target/SHA256SUMS"
