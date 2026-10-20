#!/bin/bash
# Boots a release's kernel and init under QEMU with KVM and checks what the
# init does (docs/interface.md), one machine per case. The root disk of
# the run cases is made as an agent makes it: the feature image's layers
# unpacked by the init's unpack mode onto an empty ext4 disk.
#
#   tests/boot.sh [RELEASE_DIR]
#
# RELEASE_DIR holds bzImage and initramfs.cpio.zst (default:
# out/<RELEASE>/ of this tree, as scripts/build.sh leaves it). Needs
# podman, qemu-system-x86_64, /dev/kvm, mkfs.ext4, debugfs, python3, gzip
# and zstd. There is no
# vsock device here, so the reports are read from the serial console (the
# agent's fallback); the stop command is covered by the unit tests.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
release_dir=${1:-${repo}/out/$(cat "${repo}/RELEASE")}
kernel=${release_dir}/bzImage
initrd=${release_dir}/initramfs.cpio.zst
case_timeout=30
unpack_timeout=120
init_system_timeout=60

die() {
    echo "boot.sh: $*" >&2
    exit 1
}

for tool in podman qemu-system-x86_64 mkfs.ext4 debugfs timeout python3 gzip zstd; do
    command -v "${tool}" > /dev/null || die "${tool} is not installed"
done
[[ -f ${kernel} && -f ${initrd} ]] || die "no bzImage and initramfs.cpio.zst in ${release_dir} (run scripts/build.sh)"
[[ -r /dev/kvm && -w /dev/kvm ]] || die "/dev/kvm is not usable"

work=$(mktemp -d "${TMPDIR:-/tmp}/kernel-linux-boot.XXXXXX")
cleanup() {
    rm -rf "${work}" 2> /dev/null || podman unshare rm -rf "${work}"
}
trap cleanup EXIT

# The feature image, tagged by its inputs, exported as an OCI layout: its
# layer blobs and their media types, as an agent downloads them.
features=${repo}/tests/features
tag=$(cat "${features}/Containerfile" "${features}/features.sh" "${features}/seccomp-test.c" | sha256sum | cut -c1-64)
image=localhost/kernel-linux-features:${tag}
if ! podman image exists "${image}"; then
    echo "boot.sh: building ${image}"
    podman build -q -t "${image}" "${features}" > /dev/null
fi
podman save -q --format oci-dir -o "${work}/oci" "${image}"

# layers_disk DISK CONFIG [--digest-of-last WRONG] FILE:MEDIA_TYPE...: writes
# the blobs one after the other to DISK, padded to a whole sector as a
# block device is, and the unpack config naming them to CONFIG.
layers_disk() {
    python3 - "$@" << 'PYTHON'
import hashlib, json, sys
disk, config, rest = sys.argv[1], sys.argv[2], sys.argv[3:]
wrong = None
if rest[:1] == ["--wrong-last-digest"]:
    wrong, rest = True, rest[1:]
layers = []
with open(disk, "wb") as out:
    for item in rest:
        path, media_type = item.split(":", 1)
        digest, size = hashlib.sha256(), 0
        with open(path, "rb") as blob:
            while chunk := blob.read(1 << 20):
                digest.update(chunk)
                out.write(chunk)
                size += len(chunk)
        layers.append({"media_type": media_type, "digest": "sha256:" + digest.hexdigest(), "size": size})
    out.write(b"\0" * (-out.tell() % 512))
if wrong:
    layers[-1]["digest"] = "sha256:" + "0" * 64
with open(config, "w") as out:
    json.dump({"version": 2, "mode": "unpack", "layers": layers}, out)
PYTHON
}

# The image's layers from its manifest, as FILE:MEDIA_TYPE words.
image_layers=$(python3 - "${work}/oci" << 'PYTHON'
import json, sys
oci = sys.argv[1]
def blob(digest):
    return f"{oci}/blobs/{digest.replace(':', '/')}"
index = json.load(open(f"{oci}/index.json"))
manifest = json.load(open(blob(index["manifests"][0]["digest"])))
for layer in manifest["layers"]:
    print(f"{blob(layer['digest'])}:{layer['mediaType']}")
PYTHON
)
mapfile -t image_layers <<< "${image_layers}"

# tar_layer OUT SPEC...: a layer written as an image builder writes one,
# owned by root, from SPECs: dir:PATH:MODE, file:PATH:MODE:TEXT,
# link:PATH:TARGET, chr:PATH:MAJOR:MINOR, fifo:PATH (a whiteout is a
# file named .wh.NAME); owner:UID:GID owns the entries after it so.
tar_layer() {
    python3 - "$@" << 'PYTHON'
import io, sys, tarfile
uid = gid = 0
with tarfile.open(sys.argv[1], "w", format=tarfile.PAX_FORMAT) as out:
    for spec in sys.argv[2:]:
        kind, path, *rest = spec.split(":", 3)
        if kind == "owner":
            uid, gid = int(path), int(rest[0])
            continue
        info = tarfile.TarInfo(path)
        info.uid, info.gid = uid, gid
        info.mtime = 1700000000
        data = None
        if kind == "dir":
            info.type, info.mode = tarfile.DIRTYPE, int(rest[0], 8)
        elif kind == "file":
            data = rest[1].encode()
            info.mode, info.size = int(rest[0], 8), len(data)
        elif kind == "link":
            info.type, info.linkname, info.mode = tarfile.SYMTYPE, rest[0], 0o777
        elif kind == "chr":
            info.type, info.devmajor, info.devminor, info.mode = tarfile.CHRTYPE, int(rest[0]), int(rest[1]), 0o666
        elif kind == "fifo":
            info.type, info.mode = tarfile.FIFOTYPE, 0o644
        out.addfile(info, io.BytesIO(data) if data is not None else None)
PYTHON
}

# The run cases' fixture, a last layer over the image: the disk's
# /etc/resolv.conf is a symbolic link into /run, as Debian's and Ubuntu's
# are (it dangles once /run is a tmpfs, so a mount that followed it would
# fail), its /etc/hosts has known contents and it has no /etc/hostname
# (a whiteout), so case 4 can show the init writes through none of them.
# root's ~/.ssh/authorized_keys is a symbolic link to /etc/shadow, so case
# 7 can show the keys replace the link instead of writing through it.
# Its /run/app, owned by nobody, holds a file, and /data has no vol, so
# case 12 can show the process finds the image's /run and the volume
# path. zstd-compressed, as the image's own layers are gzip.
disk_hosts='# the disk'"'"'s own hosts file'
resolv_link=../run/systemd/resolve/stub-resolv.conf
tar_layer "${work}/fixture.tar" \
    "file:etc/.wh.hostname:0644:" \
    "file:etc/hosts:0644:${disk_hosts}"$'\n' \
    "link:etc/resolv.conf:${resolv_link}" \
    "dir:root/.ssh:0700" \
    "link:root/.ssh/authorized_keys:/etc/shadow" \
    "dir:data:0755" \
    "owner:65534:65534" \
    "dir:run/app:0750" \
    "file:run/app/seed:0640:from-the-image"
zstd -q -19 --rm "${work}/fixture.tar" -o "${work}/fixture.tar.zst"
fixture=${work}/fixture.tar.zst:application/vnd.oci.image.layer.v1.tar+zstd
layers_disk "${work}/layers.img" "${work}/unpack.json" "${image_layers[@]}" "${fixture}"

network='gh.ip4=10.115.0.15/24 gh.gw4=10.115.0.1 gh.ip6=fd15:115::15/64 gh.gw6=fd15:115::1 gh.dns=10.115.0.3 gh.mtu=1500 gh.vsock=1024'
default_cmdline="console=ttyS0 quiet gh.v=1 gh.hostname=case-host ${network}"
v2_cmdline="console=ttyS0 quiet gh.v=2 gh.hostname=case-host ${network}"
v3_cmdline="console=ttyS0 quiet gh.v=3 gh.hostname=case-host ${network}"
v5_cmdline="console=ttyS0 quiet gh.v=5 gh.hostname=case-host ${network}"
unpack_cmdline=${v2_cmdline}

failures=0
case_dir=

fail() {
    echo "  FAIL: $*"
    failures=$((failures + 1))
}

# qemu CMDLINE TIMEOUT DISK [LAYERS_DISK [MEMORY_MIB]]: boots the
# machine of $case_dir with the config $case_dir/config.json; the serial
# console lands in $case_dir/serial.log, hvc0 in $case_dir/hvc.log. QEMU
# must exit by itself (the init's power-off) within TIMEOUT seconds.
qemu() {
    local cmdline=$1 timeout=$2 disk=$3 layers=${4:-} memory=${5:-256} rc=0
    local layers_args=()
    if [[ -n ${layers} ]]; then
        local drive="file=${layers},format=raw,if=none,id=layers,readonly=on"
        layers_args=(-drive "${drive}" -device "virtio-blk-pci,drive=layers")
    fi
    timeout "${timeout}" qemu-system-x86_64 \
        -M pc -enable-kvm -cpu host -m "${memory}" -smp 1 -nodefaults -no-user-config \
        -display none -no-reboot \
        -kernel "${kernel}" -initrd "${initrd}" -append "${cmdline}" \
        -serial "file:${case_dir}/serial.log" \
        -device virtio-serial-pci,id=serial0 \
        -chardev "file,id=hvc,path=${case_dir}/hvc.log" \
        -device virtconsole,chardev=hvc \
        -drive "file=${disk},format=raw,if=none,id=disk" \
        -device virtio-blk-pci,drive=disk \
        "${layers_args[@]}" \
        -netdev user,id=net0,restrict=on,ipv4=on,net=10.115.0.0/24,host=10.115.0.1,dns=10.115.0.3,ipv6=on,ipv6-net=fd15:115::/64,ipv6-host=fd15:115::1 \
        -device virtio-net-pci,netdev=net0 \
        -object rng-random,id=rng0,filename=/dev/urandom \
        -device virtio-rng-pci,rng=rng0 \
        -fw_cfg "name=opt/gigahost/config,file=${case_dir}/config.json" \
        || rc=$?
    touch "${case_dir}/serial.log" "${case_dir}/hvc.log"
    if [[ ${rc} == 124 ]]; then
        fail "${case_dir##*/}: QEMU did not exit within ${timeout} s (no power-off)"
    elif [[ ${rc} != 0 ]]; then
        fail "${case_dir##*/}: QEMU exited with ${rc}"
    fi
}

new_case() {
    case_dir=${work}/$1
    mkdir "${case_dir}"
    printf '%s' "$2" > "${case_dir}/config.json"
    echo "boot.sh: case $1"
}

# boot NAME CONFIG_JSON [CMDLINE]: boots a fresh copy of the unpacked disk
# with the config.
boot() {
    new_case "$1" "$2"
    cp --sparse=always "${work}/disk.img" "${case_dir}/disk.img"
    qemu "${3:-${default_cmdline}}" "${case_timeout}" "${case_dir}/disk.img"
}

# unpack_boot NAME CONFIG_FILE LAYERS_DISK [DISK_MIB [MEMORY_MIB [ROOT_DISK]]]:
# boots in the unpack mode onto a new, empty ext4 disk (or a copy of
# ROOT_DISK), as the agent does for a new instance; the result is
# $case_dir/disk.img.
unpack_boot() {
    new_case "$1" "$(cat "$2")"
    if [[ -n ${6:-} ]]; then
        cp --sparse=always "$6" "${case_dir}/disk.img"
    else
        mkfs.ext4 -q -F -L root -E root_owner=0:0 "${case_dir}/disk.img" "${4:-256}M" > /dev/null
    fi
    qemu "${unpack_cmdline}" "${unpack_timeout}" "${case_dir}/disk.img" "$3" "${5:-256}"
}

# The one exited report on the serial console.
exited_report() {
    grep -a -o 'gigahost-init: report {"v":[0-9]*,"event":"exited".*}' "${case_dir}/serial.log" | sed 's/^gigahost-init: report //' || true
}

expect_report() {
    local want=$1 got count
    got=$(exited_report)
    count=$(grep -a -c '"event":"exited"' "${case_dir}/serial.log" || true)
    [[ ${count} == 1 ]] || fail "expected one exited report, found ${count}"
    [[ ${got} == *"${want}"* ]] || fail "expected a report with ${want}, got: ${got:-none}"
}

expect_no_report() {
    if grep -a -q '"event":"exited"' "${case_dir}/serial.log"; then
        fail "expected no exited report, got: $(exited_report)"
    fi
}

expect_started() {
    local v=${1:-1}
    grep -a -q "gigahost-init: report {\"v\":${v},\"event\":\"started\",\"pid\":[0-9]*,\"monotonic_ms\":[0-9]*}" "${case_dir}/serial.log" \
        || fail "no started report"
}

expect_line() {
    local file=$1 line=$2
    grep -a -q -x -F -- "${line}" "${case_dir}/${file}" || fail "${file} lacks the line: ${line}"
}

expect_absent() {
    local file=$1 text=$2
    if grep -a -q -F -- "${text}" "${case_dir}/${file}"; then
        fail "${file} shows ${text}"
    fi
}

show_logs() {
    if [[ ${failures} != "$1" ]]; then
        echo "  --- serial console ---"
        sed 's/^/  | /' "${case_dir}/serial.log" | tail -n 40
        echo "  --- hvc0 ---"
        sed 's/^/  | /' "${case_dir}/hvc.log" | tail -n 40
    fi
}

# debugfs_ls DISK DIR: the names in DIR on the disk, sorted, one a line.
debugfs_ls() {
    debugfs -R "ls -p $2" "$1" 2> /dev/null | awk -F/ 'NF > 6 && $6 != "." && $6 != ".." { print $6 }' | sort
}

# U1. The unpack mode: the feature image's layers and the fixture onto an
# empty disk, a version-2 report, the power-off; the file capability and
# the setuid bit are kept. The run cases below boot copies of this disk.
before=${failures}
unpack_boot unpack "${work}/unpack.json" "${work}/layers.img"
expect_report '{"v":2,"event":"exited","code":0,"signal":null,"reason":"unpacked"}'
expect_absent serial.log '"event":"started"'
cp --sparse=always "${case_dir}/disk.img" "${work}/disk.img"
caps=$(debugfs -R 'ea_list /usr/local/bin/ping-cap' "${work}/disk.img" 2> /dev/null)
[[ ${caps} == *security.capability* ]] || fail "ping-cap lost its security.capability: ${caps}"
setuid=$(debugfs -R 'stat /usr/local/bin/setuid-test' "${work}/disk.img" 2> /dev/null)
[[ ${setuid} == *"Mode:  04755"* && ${setuid} == *"User:     0"* ]] \
    || fail "setuid-test is not mode 04755 owned by root: ${setuid}"
debugfs -R 'stat /etc/hostname' "${work}/disk.img" 2>&1 | grep -q 'File not found' \
    || fail "the fixture's whiteout left /etc/hostname"
show_logs "${before}"
disk_shadow=$(debugfs -R 'cat /etc/shadow' "${work}/disk.img" 2> /dev/null)
[[ -n ${disk_shadow} ]] || die "the disk has no /etc/shadow"

# U2. A whiteout and an opaque directory across two layers, as the OCI
# rules have them; device nodes and FIFOs are made.
before=${failures}
tar_layer "${work}/lower.tar" \
    "dir:wo:0755" "file:wo/a/x:0644:x" "file:wo/a/sub/y:0644:y" \
    "file:wo/b/z:0644:z" "file:wo/gone:0644:g" "file:wo/keep:0644:k"
tar_layer "${work}/upper.tar" \
    "file:wo/a/early:0644:e" "file:wo/a/.wh..wh..opq:0644:" "file:wo/a/new:0644:n" \
    "file:wo/.wh.b:0644:" "file:wo/.wh.gone:0644:" "chr:wo/null:1:3" "fifo:wo/fifo"
gzip -n -9 "${work}/upper.tar"
layers_disk "${work}/wo-layers.img" "${work}/wo.json" \
    "${work}/lower.tar:application/vnd.oci.image.layer.v1.tar" \
    "${work}/upper.tar.gz:application/vnd.docker.image.rootfs.diff.tar.gzip"
unpack_boot whiteouts "${work}/wo.json" "${work}/wo-layers.img" 64
expect_report '"reason":"unpacked"'
[[ $(debugfs_ls "${case_dir}/disk.img" /wo | tr '\n' ' ') == 'a fifo keep null ' ]] \
    || fail "/wo holds: $(debugfs_ls "${case_dir}/disk.img" /wo | tr '\n' ' ')"
[[ $(debugfs_ls "${case_dir}/disk.img" /wo/a | tr '\n' ' ') == 'early new ' ]] \
    || fail "/wo/a holds: $(debugfs_ls "${case_dir}/disk.img" /wo/a | tr '\n' ' ')"
null=$(debugfs -R 'stat /wo/null' "${case_dir}/disk.img" 2> /dev/null)
[[ ${null} == *"Type: character special"* && ${null} == *"Device major/minor number: 01:03"* ]] \
    || fail "/wo/null is not the character device 1:3: ${null}"
show_logs "${before}"

# U3. A wrong digest: an error naming the layer, nothing started.
before=${failures}
layers_disk "${work}/bad-layers.img" "${work}/bad.json" --wrong-last-digest "${image_layers[@]}" "${fixture}"
unpack_boot wrong-digest "${work}/bad.json" "${work}/bad-layers.img"
expect_report "\"reason\":\"error\",\"message\":\"layer ${#image_layers[@]}: its digest does not match\""
expect_absent serial.log '"event":"started"'
show_logs "${before}"

# U4. A root disk that is not empty: an error, the disk unchanged.
before=${failures}
unpack_boot not-empty "${work}/wo.json" "${work}/wo-layers.img" "" "" "${work}/disk.img"
expect_report '"v":2,"event":"exited","code":null,"signal":null,"reason":"error","message":"the root disk is not empty"'
cmp -s "${case_dir}/disk.img" "${work}/disk.img" || fail "the non-empty disk changed"
show_logs "${before}"

# U5. A layer larger than the guest's memory: 512 MiB that do not
# compress, at 192 MiB of RAM.
before=${failures}
mkdir "${work}/big"
head -c 512M /dev/urandom > "${work}/big/big"
tar --owner=0 --group=0 --numeric-owner -C "${work}/big" -cf - big | gzip -1 > "${work}/big.tar.gz"
layers_disk "${work}/big-layers.img" "${work}/big.json" "${work}/big.tar.gz:application/vnd.oci.image.layer.v1.tar+gzip"
rm -f "${work}/big.tar.gz"
unpack_boot big-layer "${work}/big.json" "${work}/big-layers.img" 1024 192
expect_report '"reason":"unpacked"'
rm -f "${work}/big-layers.img"
debugfs -R "dump /big ${case_dir}/big.out" "${case_dir}/disk.img" 2> /dev/null
cmp -s "${case_dir}/big.out" "${work}/big/big" || fail "the unpacked 512 MiB file differs"
rm -rf "${work}/big" "${case_dir}"
show_logs "${before}"

# U6–U11. Version 4's redeploy. Image A is the feature image with the
# fixture and a layer of its own (with /vol, a symbolic link to /data);
# image B is the feature image with another, whose /data is opaque and
# has a /data/db/file of its own.
v4_cmdline="console=ttyS0 quiet gh.v=4 gh.hostname=case-host ${network}"
# unpack_config IN OUT VERSION [KEEP_JSON]: IN's unpack config in VERSION,
# with "keep": KEEP_JSON when given.
unpack_config() {
    python3 - "$@" << 'PYTHON'
import json, sys
config = json.load(open(sys.argv[1]))
config["version"] = int(sys.argv[3])
if len(sys.argv) > 4:
    config["keep"] = json.loads(sys.argv[4])
json.dump(config, open(sys.argv[2], "w"))
PYTHON
}
tar_layer "${work}/a.tar" \
    "dir:data:0755" "file:data/db/file:0644:A" "file:etc/a-only:0644:a" \
    "file:usr/bin/tool:0755:a-tool" "link:vol:/data"
tar_layer "${work}/b.tar" \
    "file:data/.wh..wh..opq:0644:" "file:data/db/file:0644:B" "file:etc/b-only:0644:b" \
    "file:usr/local/bin/b-process:0755:echo image-b; cat /data/db/file; echo; ls /etc/a-only /usr/bin/tool 2>&1 || true"
layers_disk "${work}/a-layers.img" "${work}/a.json" "${image_layers[@]}" "${fixture}" \
    "${work}/a.tar:application/vnd.oci.image.layer.v1.tar"
layers_disk "${work}/b-layers.img" "${work}/b.json" "${image_layers[@]}" \
    "${work}/b.tar:application/vnd.oci.image.layer.v1.tar"
before=${failures}
unpack_boot image-a "${work}/a.json" "${work}/a-layers.img"
expect_report '"reason":"unpacked"'
mv "${case_dir}/disk.img" "${work}/a.img"
rm -f "${work}/a-layers.img"
show_logs "${before}"

# U6. A redeploy to image B keeping /data: /data is A's, everything else
# B's; lost+found stays; a version-4 report.
before=${failures}
unpack_config "${work}/b.json" "${work}/b-keep.json" 4 '["/data"]'
unpack_cmdline=${v4_cmdline}
unpack_boot redeploy "${work}/b-keep.json" "${work}/b-layers.img" "" "" "${work}/a.img"
unpack_cmdline=${v2_cmdline}
expect_report '{"v":4,"event":"exited","code":0,"signal":null,"reason":"unpacked"}'
grep -a -q 'gigahost-init: cleared the root disk but for the kept paths' "${case_dir}/serial.log" \
    || fail "no line saying the root was cleared"
grep -a -q -E "gigahost-init: layer ${#image_layers[@]}: kept paths: 2 entries skipped" "${case_dir}/serial.log" \
    || fail "no line counting B's skipped entries"
[[ $(debugfs -R 'cat /data/db/file' "${case_dir}/disk.img" 2> /dev/null) == A ]] \
    || fail "/data/db/file is not A's"
for gone in /etc/a-only /usr/bin/tool; do
    debugfs -R "stat ${gone}" "${case_dir}/disk.img" 2>&1 | grep -q 'File not found' \
        || fail "A's ${gone} survived the redeploy"
done
[[ $(debugfs -R 'cat /etc/b-only' "${case_dir}/disk.img" 2> /dev/null) == b ]] || fail "no /etc/b-only"
debugfs_ls "${case_dir}/disk.img" /usr/bin | grep -q -x env || fail "B's /usr/bin is missing"
debugfs_ls "${case_dir}/disk.img" / | grep -q -x lost+found || fail "lost+found is gone"
mv "${case_dir}/disk.img" "${work}/redeployed.img"
show_logs "${before}"

# U7. The run boot after the redeploy starts B's process, on A's /data.
before=${failures}
new_case redeployed-run '{"version":4,"argv":["/bin/sh","/usr/local/bin/b-process"]}'
mv "${work}/redeployed.img" "${case_dir}/disk.img"
qemu "${v4_cmdline}" "${case_timeout}" "${case_dir}/disk.img"
expect_started 4
tr -d '\r' < "${case_dir}/hvc.log" > "${case_dir}/console.txt"
expect_line console.txt image-b
expect_line console.txt A
expect_absent console.txt a-tool
expect_report '{"v":4,"event":"exited","code":0,"signal":null,"reason":"exited"}'
rm -f "${case_dir}/disk.img"
show_logs "${before}"

# U8. A redeploy keeping nothing replaces everything.
before=${failures}
unpack_config "${work}/b.json" "${work}/b-nothing.json" 4 '[]'
unpack_cmdline=${v4_cmdline}
unpack_boot redeploy-nothing "${work}/b-nothing.json" "${work}/b-layers.img" "" "" "${work}/a.img"
unpack_cmdline=${v2_cmdline}
expect_report '{"v":4,"event":"exited","code":0,"signal":null,"reason":"unpacked"}'
[[ $(debugfs -R 'cat /data/db/file' "${case_dir}/disk.img" 2> /dev/null) == B ]] \
    || fail "/data/db/file is not B's"
debugfs -R 'stat /etc/a-only' "${case_dir}/disk.img" 2>&1 | grep -q 'File not found' \
    || fail "A's /etc/a-only survived the redeploy"
rm -f "${case_dir}/disk.img"
show_logs "${before}"

# U9. keep in a version-3 config: an unknown field, the disk unchanged.
before=${failures}
unpack_config "${work}/b.json" "${work}/b-keep-v3.json" 3 '["/data"]'
unpack_cmdline=${v3_cmdline}
unpack_boot keep-v3 "${work}/b-keep-v3.json" "${work}/b-layers.img" "" "" "${work}/a.img"
unpack_cmdline=${v2_cmdline}
expect_report "\"v\":3,\"event\":\"exited\",\"code\":null,\"signal\":null,\"reason\":\"error\",\"message\":\"the config has an unknown field \`keep\`\""
cmp -s "${case_dir}/disk.img" "${work}/a.img" || fail "the disk changed"
rm -f "${case_dir}/disk.img"
show_logs "${before}"

# U10. A redeploy keeping a path that a symbolic link leads elsewhere
# (/vol/db, which is A's /data/db) is refused by the path's index, while
# the disk is read-only: the disk is unchanged.
before=${failures}
unpack_config "${work}/b.json" "${work}/b-keep-link.json" 4 '["/data","/vol/db"]'
unpack_cmdline=${v4_cmdline}
unpack_boot keep-link "${work}/b-keep-link.json" "${work}/b-layers.img" "" "" "${work}/a.img"
unpack_cmdline=${v2_cmdline}
expect_report '"v":4,"event":"exited","code":null,"signal":null,"reason":"error","message":"kept path 1 passes through a symbolic link"'
cmp -s "${case_dir}/disk.img" "${work}/a.img" || fail "the disk changed"
rm -f "${case_dir}/disk.img"
show_logs "${before}"

# U11. A version-4 unpack without keep still refuses a disk that is not
# empty, unchanged.
before=${failures}
unpack_config "${work}/b.json" "${work}/b-v4.json" 4
unpack_cmdline=${v4_cmdline}
unpack_boot not-empty-v4 "${work}/b-v4.json" "${work}/b-layers.img" "" "" "${work}/a.img"
unpack_cmdline=${v2_cmdline}
expect_report '"v":4,"event":"exited","code":null,"signal":null,"reason":"error","message":"the root disk is not empty"'
cmp -s "${case_dir}/disk.img" "${work}/a.img" || fail "the non-empty disk changed"
rm -f "${case_dir}/disk.img" "${work}/a.img" "${work}/b-layers.img"
show_logs "${before}"

# 1. The kernel feature checks pass and the process exits 0.
before=${failures}
boot features '{"version":1,"argv":["/bin/sh","/usr/local/bin/features.sh"]}'
expect_started
passed=$(grep -a -c '^FEATURE [a-z0-9_]* PASS' "${case_dir}/hvc.log" || true)
[[ ${passed} == 21 ]] || fail "expected 21 passing feature checks, got ${passed}"
expect_absent hvc.log ' FAIL '
expect_report '"code":0,"signal":null,"reason":"exited"'
show_logs "${before}"

# 2. Exit codes and signals; the run mode of a version-2 boot, whose
# reports say version 2.
before=${failures}
boot exit-3 '{"version":1,"argv":["/bin/sh","-c","exit 3"]}'
expect_report '{"v":1,"event":"exited","code":3,"signal":null,"reason":"exited"}'
show_logs "${before}"
before=${failures}
boot exit-3-v2 '{"version":2,"mode":"run","argv":["/bin/sh","-c","exit 3"]}' "${v2_cmdline}"
expect_started 2
expect_report '{"v":2,"event":"exited","code":3,"signal":null,"reason":"exited"}'
show_logs "${before}"
before=${failures}
boot kill-9 '{"version":1,"argv":["/bin/sh","-c","kill -9 $$"]}'
expect_report '"code":null,"signal":9,"reason":"exited"'
show_logs "${before}"

# 3. The user, a working directory that is created, the environment; the
# value never reaches the serial console.
before=${failures}
secret=bar-5c1f0e7a
config=$(cat << 'JSON'
{"version":1,
 "argv":["/bin/sh","-c","id -u; id -g; pwd; echo \"FOO=$FOO\"; echo \"HOME=$HOME\""],
 "env":["FOO=bar-5c1f0e7a"],"user":"nobody","workdir":"/tmp/w"}
JSON
)
boot user "${config}"
expect_started
expect_line hvc.log 65534
expect_line hvc.log /tmp/w
expect_line hvc.log "FOO=${secret}"
expect_line hvc.log 'HOME=/'
expect_absent serial.log "${secret}"
expect_report '"code":0,"signal":null,"reason":"exited"'
show_logs "${before}"

# 4. The host name, /etc/hosts and /etc/resolv.conf inside are ours, even
# written to; the disk's own /etc/hosts and its resolv.conf link, which
# dangles inside (a mount that followed it would have failed), are
# unchanged.
before=${failures}
boot identity '{"version":1,"argv":["/bin/sh","-c","hostname; cat /etc/hostname /etc/hosts /etc/resolv.conf; echo changed > /etc/resolv.conf; echo changed > /etc/hosts; ls /run/systemd 2>&1 || echo no-run-systemd"]}'
expect_line hvc.log case-host
expect_line hvc.log "$(printf '10.115.0.15\tcase-host')"
expect_line hvc.log "$(printf 'fd15:115::15\tcase-host')"
expect_line hvc.log 'nameserver 10.115.0.3'
expect_line hvc.log no-run-systemd
expect_report '"code":0,"signal":null,"reason":"exited"'
link=$(debugfs -R 'stat /etc/resolv.conf' "${case_dir}/disk.img" 2> /dev/null)
[[ ${link} == *"Type: symlink"* && ${link} == *"Fast link dest: \"${resolv_link}\""* ]] \
    || fail "the disk's /etc/resolv.conf is no longer its symbolic link"
[[ $(debugfs -R 'cat /etc/hosts' "${case_dir}/disk.img" 2> /dev/null) == "${disk_hosts}" ]] \
    || fail "the disk's /etc/hosts changed"
show_logs "${before}"

# 5. A config of another version, one with an unknown field, and another
# command-line version: an error report, no process, a power-off.
before=${failures}
boot config-v2 '{"version":2,"argv":["/bin/sh","-c","echo ran"]}'
expect_report '"reason":"error","message":"the config'"'"'s version is not one this init supports"'
expect_absent serial.log '"event":"started"'
expect_absent hvc.log ran
show_logs "${before}"
before=${failures}
boot config-unknown '{"version":1,"argv":["/bin/sh","-c","echo ran"],"surprise":true}'
expect_report "\"reason\":\"error\",\"message\":\"the config has an unknown field \`surprise\`\""
expect_absent serial.log '"event":"started"'
expect_absent hvc.log ran
show_logs "${before}"
before=${failures}
boot cmdline-v6 '{"version":1,"argv":["/bin/sh","-c","echo ran"]}' "console=ttyS0 quiet gh.v=6 gh.hostname=case-host ${network}"
expect_report '"reason":"error","message":"gh.v names an interface version this init does not support"'
expect_absent serial.log '"event":"started"'
show_logs "${before}"
before=${failures}
boot config-v1-on-v2 '{"version":1,"argv":["/bin/sh","-c","echo ran"]}' "${v2_cmdline}"
expect_report '{"v":2,"event":"exited","code":null,"signal":null,"reason":"error","message":"the config'"'"'s version is not the boot'"'"'s interface version (gh.v)"}'
expect_absent serial.log '"event":"started"'
expect_absent hvc.log ran
show_logs "${before}"

# 6. The process's output goes to hvc0, the init's own lines to the serial
# console.
before=${failures}
boot output '{"version":1,"argv":["/bin/sh","-c","echo to-stdout; echo to-stderr >&2"]}'
expect_line hvc.log to-stdout
expect_line hvc.log to-stderr
expect_absent serial.log to-stdout
expect_absent serial.log to-stderr
grep -a -q '^gigahost-init: start [0-9]*' "${case_dir}/serial.log" || fail "no start line on the serial console"
expect_absent hvc.log gigahost-init
expect_report '"code":0,"signal":null,"reason":"exited"'
show_logs "${before}"

# 7. The SSH keys replace root's authorized_keys, a symbolic link to
# /etc/shadow, instead of writing through it.
before=${failures}
boot authorized-keys '{"version":1,"argv":["/bin/true"],"authorized_keys":{"user":"root","keys":["ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK5w case-key"]}}'
expect_report '"code":0,"signal":null,"reason":"exited"'
[[ $(debugfs -R 'cat /etc/shadow' "${case_dir}/disk.img" 2> /dev/null) == "${disk_shadow}" ]] \
    || fail "the disk's /etc/shadow changed"
keys=$(debugfs -R 'stat /root/.ssh/authorized_keys' "${case_dir}/disk.img" 2> /dev/null)
[[ ${keys} == *"Type: regular"* && ${keys} == *"Mode:  0600"* ]] \
    || fail "authorized_keys is not a regular file of mode 0600: ${keys}"
[[ $(debugfs -R 'cat /root/.ssh/authorized_keys' "${case_dir}/disk.img" 2> /dev/null) == 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK5w case-key' ]] \
    || fail "authorized_keys does not hold the key"
show_logs "${before}"

# 8. The process starts with every signal at its default action and none
# blocked, as under a container runtime: a pipeline's writer dies of
# SIGPIPE quietly. (grep reads its own status: busybox's sh ignores
# SIGQUIT itself, as bash does, and gives its commands what it
# inherited.)
before=${failures}
boot signals '{"version":1,"argv":["/bin/sh","-c","grep -E \"^Sig(Ign|Blk):\" /proc/self/status; yes | head -n 1"]}'
expect_line hvc.log "$(printf 'SigBlk:\t0000000000000000')"
expect_line hvc.log "$(printf 'SigIgn:\t0000000000000000')"
expect_line hvc.log y
expect_absent hvc.log 'Broken pipe'
expect_report '"code":0,"signal":null,"reason":"exited"'
show_logs "${before}"

# 9. A kernel panic and a reboot by the process end the machine at once
# (the panic resets it, -no-reboot ends QEMU on a reset), without an exited
# report.
before=${failures}
boot panic '{"version":1,"argv":["/bin/sh","-c","echo c > /proc/sysrq-trigger"]}'
grep -a -q 'Kernel panic - not syncing' "${case_dir}/serial.log" || fail "no kernel panic on the serial console"
expect_no_report
show_logs "${before}"
before=${failures}
boot reboot '{"version":1,"argv":["/bin/sh","-c","reboot -f"]}'
expect_no_report
show_logs "${before}"

# 10a. A disk larger than its ext4 file system (a disk made or resized
# larger than the image's): the init grows the file system online to the
# disk's size before the process starts, which sees the space. A boot of
# a disk the file system already fills (every other case, and the grown
# disk again) changes nothing and logs nothing about it.
root_kib_config=$(cat << 'JSON'
{"version":1,"argv":["/bin/sh","-c","df -Pk / | awk 'NR == 2 { print \"ROOT_KIB=\" $2 }'"]}
JSON
)
# root_kib: the root's size in KiB as the process saw it, 0 for none.
root_kib() {
    local line
    line=$(tr -d '\r' < "${case_dir}/hvc.log" | grep -a -m 1 '^ROOT_KIB=[0-9]*$' || true)
    line=${line#ROOT_KIB=}
    echo "${line:-0}"
}
before=${failures}
new_case grow "${root_kib_config}"
cp --sparse=always "${work}/disk.img" "${case_dir}/disk.img"
truncate -s 1G "${case_dir}/disk.img"
qemu "${default_cmdline}" "${case_timeout}" "${case_dir}/disk.img"
expect_started
expect_report '"code":0,"signal":null,"reason":"exited"'
grep -a -q -E '^gigahost-init: grew the root file system from [0-9]+ to [0-9]+ blocks' "${case_dir}/serial.log" \
    || fail "no line saying the root grew"
expect_absent serial.log 'growing the root file system failed'
grown_kib=$(root_kib)
(( grown_kib > 900000 )) || fail "the grown root has ${grown_kib} KiB, expected more than 900000"
mv "${case_dir}/disk.img" "${work}/grown.img"
show_logs "${before}"
before=${failures}
new_case grown-again "${root_kib_config}"
mv "${work}/grown.img" "${case_dir}/disk.img"
qemu "${default_cmdline}" "${case_timeout}" "${case_dir}/disk.img"
expect_report '"code":0,"signal":null,"reason":"exited"'
expect_absent serial.log 'grew the root file system'
expect_absent serial.log 'growing the root file system failed'
[[ $(root_kib) == "${grown_kib}" ]] || fail "the root has $(root_kib) KiB on its second boot, ${grown_kib} on its first"
rm -f "${case_dir}/disk.img"
show_logs "${before}"
before=${failures}
new_case no-grow "${root_kib_config}"
cp --sparse=always "${work}/disk.img" "${case_dir}/disk.img"
qemu "${default_cmdline}" "${case_timeout}" "${case_dir}/disk.img"
expect_report '"code":0,"signal":null,"reason":"exited"'
expect_absent serial.log 'grew the root file system'
expect_absent serial.log 'growing the root file system failed'
kib=$(root_kib)
(( kib > 200000 && kib < 270000 )) || fail "the root of a 256 MiB disk has ${kib} KiB"
show_logs "${before}"
before=${failures}
new_case grow-refused "${root_kib_config}"
cp --sparse=always "${work}/disk.img" "${case_dir}/disk.img"
debugfs -w -R 'ssv state 3' "${case_dir}/disk.img" > /dev/null 2>&1
truncate -s 1G "${case_dir}/disk.img"
qemu "${default_cmdline}" "${case_timeout}" "${case_dir}/disk.img"
expect_started
expect_report '"code":0,"signal":null,"reason":"exited"'
grep -a -q 'growing the root file system failed: ' "${case_dir}/serial.log" \
    || fail "no line saying the growth failed"
expect_absent serial.log 'grew the root file system'
show_logs "${before}"

# 10. Version 3 without init_system is version 2's run mode; init_system
# is refused on a version-2 boot and with a user other than root.
before=${failures}
boot exit-3-v3 '{"version":3,"argv":["/bin/sh","-c","exit 3"]}' "${v3_cmdline}"
expect_started 3
expect_report '{"v":3,"event":"exited","code":3,"signal":null,"reason":"exited"}'
show_logs "${before}"
before=${failures}
boot init-system-v2 '{"version":2,"argv":["/sbin/init"],"init_system":true}' "${v2_cmdline}"
expect_report "\"reason\":\"error\",\"message\":\"the config has an unknown field \`init_system\`\""
expect_absent serial.log '"event":"started"'
show_logs "${before}"
before=${failures}
boot init-system-nobody '{"version":3,"argv":["/sbin/init"],"init_system":true,"user":"nobody"}' "${v3_cmdline}"
expect_report "\"reason\":\"error\",\"message\":\"config field \`user\` is not root, which \`init_system\` requires\""
expect_absent serial.log '"event":"started"'
show_logs "${before}"

# 12. The image's /run is the process's (seeded into the tmpfs, owners
# and modes kept, writable, the disk's own unchanged), and version 5's
# volumes: the missing /data/vol is made (root, 0755), the existing
# /run/app left alone. volumes in a version-4 config is refused.
before=${failures}
config=$(cat << 'JSON'
{"version":5,"user":"nobody","volumes":["/data/vol","/run/app"],
 "argv":["/bin/sh","-c","stat -c 'RUN_APP=%u:%g:%a' /run/app; stat -c 'SEED=%u:%a' /run/app/seed; cat /run/app/seed; echo; echo new > /run/app/new && cat /run/app/new; stat -c 'VOL=%u:%g:%a' /data/vol; test -f /run/gigahost/hosts && echo OURS"]}
JSON
)
boot run-and-volumes "${config}" "${v5_cmdline}"
expect_started 5
tr -d '\r' < "${case_dir}/hvc.log" > "${case_dir}/console.txt"
expect_line console.txt RUN_APP=65534:65534:750
expect_line console.txt SEED=65534:640
expect_line console.txt from-the-image
expect_line console.txt new
expect_line console.txt VOL=0:0:755
expect_line console.txt OURS
grep -a -q 'gigahost-init: made 1 missing volume path(s)' "${case_dir}/serial.log" \
    || fail "no line saying the volume path was made"
expect_absent serial.log "copying the image's /run stopped"
expect_report '{"v":5,"event":"exited","code":0,"signal":null,"reason":"exited"}'
debugfs -R 'stat /run/app/new' "${case_dir}/disk.img" 2>&1 | grep -q 'File not found' \
    || fail "the process wrote the disk's /run"
show_logs "${before}"
before=${failures}
boot volumes-v4 '{"version":4,"argv":["/bin/sh","-c","echo ran"],"volumes":["/data/vol"]}' "${v4_cmdline}"
expect_report "\"v\":4,\"event\":\"exited\",\"code\":null,\"signal\":null,\"reason\":\"error\",\"message\":\"the config has an unknown field \`volumes\`\""
expect_absent serial.log '"event":"started"'
expect_absent hvc.log ran
show_logs "${before}"

# 11. Version 3's init_system: an image's own init system (systemd on
# Debian, busybox init running OpenRC on Alpine) boots as PID 1 of its own
# namespaces, its checks pass (tests/init-systems/init-features.sh), and it
# powers the container off from inside, which the report says; a second
# boot reboots from inside, which the report says too. The image is
# unpacked by a version-3 unpack boot.
init_systems=${repo}/tests/init-systems
# init_system_disk NAME CONTAINERFILE: builds the image and unpacks it
# onto ${work}/NAME.img.
init_system_disk() {
    local name=$1 containerfile=$2 tag layers before=${failures}
    tag=$(cat "${init_systems}/${containerfile}" "${init_systems}"/init-features.* | sha256sum | cut -c1-64)
    local image=localhost/kernel-linux-${name}:${tag}
    if ! podman image exists "${image}"; then
        echo "boot.sh: building ${image}"
        podman build -q -t "${image}" -f "${init_systems}/${containerfile}" "${init_systems}" > /dev/null
    fi
    podman save -q --format oci-dir -o "${work}/${name}-oci" "${image}"
    layers=$(python3 - "${work}/${name}-oci" << 'PYTHON'
import json, sys
oci = sys.argv[1]
def blob(digest):
    return f"{oci}/blobs/{digest.replace(':', '/')}"
index = json.load(open(f"{oci}/index.json"))
manifest = json.load(open(blob(index["manifests"][0]["digest"])))
for layer in manifest["layers"]:
    print(f"{blob(layer['digest'])}:{layer['mediaType']}")
PYTHON
    )
    local image_layers=()
    mapfile -t image_layers <<< "${layers}"
    layers_disk "${work}/${name}-layers.img" "${work}/${name}-unpack.json" "${image_layers[@]}"
    sed -i 's/^{"version": 2,/{"version": 3,/' "${work}/${name}-unpack.json"
    rm -rf "${work}/${name}-oci"
    unpack_cmdline=${v3_cmdline}
    unpack_boot "${name}-unpack" "${work}/${name}-unpack.json" "${work}/${name}-layers.img" 1024
    unpack_cmdline=${v2_cmdline}
    expect_report '{"v":3,"event":"exited","code":0,"signal":null,"reason":"unpacked"}'
    mv "${case_dir}/disk.img" "${work}/${name}.img"
    rm -f "${work}/${name}-layers.img"
    show_logs "${before}"
}

# init_system_boot NAME DISK CONFIG HOSTNAME: boots a copy of DISK with
# CONFIG on a version-3 command line; the console's text, without the
# carriage returns the image's init may set its terminal to add, lands in
# $case_dir/console.txt.
init_system_boot() {
    new_case "$1" "$3"
    cp --sparse=always "$2" "${case_dir}/disk.img"
    qemu "${v3_cmdline/gh.hostname=case-host/gh.hostname=$4}" "${init_system_timeout}" "${case_dir}/disk.img"
    tr -d '\r' < "${case_dir}/hvc.log" > "${case_dir}/console.txt"
}

# expect_init_features COUNT: COUNT passing checks and no failing one.
# An image's init that never ran the checks prints no INFO line, which
# must not end the script before the case's logs are shown.
expect_init_features() {
    local passed
    passed=$(grep -a -c '^FEATURE [a-z0-9_]* PASS$' "${case_dir}/console.txt" || true)
    [[ ${passed} == "$1" ]] || fail "expected $1 passing init-system checks, got ${passed}"
    expect_absent console.txt ' FAIL'
    grep -a '^INFO ' "${case_dir}/console.txt" | sed 's/^/  /' || true
}

# init_system_cases NAME CONTAINERFILE STOP_SIGNAL CHECKS
init_system_cases() {
    local name=$1 before
    init_system_disk "${name}" "$2"
    local config='{"version":3,"argv":["/sbin/init"],"init_system":true,"stop_signal":'"$3"'}'
    before=${failures}
    init_system_boot "${name}-poweroff" "${work}/${name}.img" "${config}" poweroff-host
    expect_started 3
    expect_init_features "$4"
    expect_line console.txt 'INFO action poweroff'
    expect_report '{"v":3,"event":"exited","code":null,"signal":2,"reason":"poweroff"}'
    show_logs "${before}"
    before=${failures}
    init_system_boot "${name}-reboot" "${work}/${name}.img" "${config}" reboot-host
    expect_started 3
    expect_init_features "$4"
    expect_line console.txt 'INFO action reboot'
    expect_report '{"v":3,"event":"exited","code":null,"signal":1,"reason":"reboot"}'
    show_logs "${before}"
    rm -f "${work}/${name}.img"
}

# systemd stops on SIGRTMIN+3 (37 with glibc), busybox init powers off on
# SIGUSR2 (12).
init_system_cases systemd Containerfile.debian 37 12
init_system_cases openrc Containerfile.alpine 12 11

if [[ ${failures} != 0 ]]; then
    echo "boot.sh: ${failures} check(s) failed"
    exit 1
fi
echo "boot.sh: all cases passed"
