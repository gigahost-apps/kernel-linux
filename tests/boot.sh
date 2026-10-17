#!/bin/bash
# Boots a release's kernel and init under QEMU with KVM and checks what the
# init does (docs/interface.md), one machine per case:
#
#   tests/boot.sh [RELEASE_DIR]
#
# RELEASE_DIR holds bzImage and initramfs.cpio.zst (default:
# out/<RELEASE>/ of this tree, as scripts/build.sh leaves it). Needs
# podman, qemu-system-x86_64, /dev/kvm, mkfs.ext4 and debugfs. There is no
# vsock device here, so the reports are read from the serial console (the
# agent's fallback); the stop command is covered by the unit tests.
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
release_dir=${1:-${repo}/out/$(cat "${repo}/RELEASE")}
kernel=${release_dir}/bzImage
initrd=${release_dir}/initramfs.cpio.zst
case_timeout=30

die() {
    echo "boot.sh: $*" >&2
    exit 1
}

for tool in podman qemu-system-x86_64 mkfs.ext4 debugfs timeout; do
    command -v "${tool}" > /dev/null || die "${tool} is not installed"
done
[[ -f ${kernel} && -f ${initrd} ]] || die "no bzImage and initramfs.cpio.zst in ${release_dir} (run scripts/build.sh)"
[[ -r /dev/kvm && -w /dev/kvm ]] || die "/dev/kvm is not usable"

work=$(mktemp -d "${TMPDIR:-/tmp}/kernel-linux-boot.XXXXXX")
cleanup() {
    rm -rf "${work}" 2> /dev/null || podman unshare rm -rf "${work}"
}
trap cleanup EXIT

# The feature image, tagged by its inputs, and a root disk made from it.
# The disk's /etc/resolv.conf is a symbolic link into /run, as Debian's and
# Ubuntu's are (it dangles once /run is a tmpfs, so a mount that followed
# it would fail), its /etc/hosts has known contents and it has no
# /etc/hostname, so case 4 can show the init writes through none of them.
# root's ~/.ssh/authorized_keys is a symbolic link to /etc/shadow, so case
# 7 can show the keys replace the link instead of writing through it.
features=${repo}/tests/features
tag=$(cat "${features}/Containerfile" "${features}/features.sh" "${features}/seccomp-test.c" | sha256sum | cut -c1-64)
image=localhost/kernel-linux-features:${tag}
if ! podman image exists "${image}"; then
    echo "boot.sh: building ${image}"
    podman build -q -t "${image}" "${features}" > /dev/null
fi
container=$(podman create "${image}" /bin/true)
podman export -o "${work}/rootfs.tar" "${container}"
podman rm "${container}" > /dev/null
disk_hosts='# the disk'"'"'s own hosts file'
resolv_link=../run/systemd/resolve/stub-resolv.conf
podman unshare sh -euc '
    root=$1/root
    mkdir "$root"
    tar -C "$root" -xf "$1/rootfs.tar"
    rm -f "$root/etc/resolv.conf" "$root/etc/hosts" "$root/etc/hostname"
    printf "%s\n" "$2" > "$root/etc/hosts"
    ln -s "$3" "$root/etc/resolv.conf"
    mkdir -p -m 0700 "$root/root/.ssh"
    ln -s /etc/shadow "$root/root/.ssh/authorized_keys"
    mkfs.ext4 -q -F -L root -E root_owner=0:0 -d "$root" "$1/disk.img" 256M
    rm -rf "$root"
' sh "${work}" "${disk_hosts}" "${resolv_link}"
rm -f "${work}/rootfs.tar"
disk_shadow=$(debugfs -R 'cat /etc/shadow' "${work}/disk.img" 2> /dev/null)
[[ -n ${disk_shadow} ]] || die "the disk has no /etc/shadow"

network='gh.ip4=10.115.0.15/24 gh.gw4=10.115.0.1 gh.ip6=fd15:115::15/64 gh.gw6=fd15:115::1 gh.dns=10.115.0.3 gh.mtu=1500 gh.vsock=1024'
default_cmdline="console=ttyS0 quiet gh.v=1 gh.hostname=case-host ${network}"

failures=0
case_dir=

fail() {
    echo "  FAIL: $*"
    failures=$((failures + 1))
}

# boot NAME CONFIG_JSON [CMDLINE]: boots a fresh copy of the disk with the
# config; the serial console lands in $case_dir/serial.log, hvc0 in
# $case_dir/hvc.log. QEMU must exit by itself (the init's power-off).
boot() {
    local name=$1 config=$2 cmdline=${3:-${default_cmdline}}
    case_dir=${work}/${name}
    mkdir "${case_dir}"
    printf '%s' "${config}" > "${case_dir}/config.json"
    cp --sparse=always "${work}/disk.img" "${case_dir}/disk.img"
    echo "boot.sh: case ${name}"
    local rc=0
    timeout "${case_timeout}" qemu-system-x86_64 \
        -M pc -enable-kvm -cpu host -m 256 -smp 1 -nodefaults -no-user-config \
        -display none -no-reboot \
        -kernel "${kernel}" -initrd "${initrd}" -append "${cmdline}" \
        -serial "file:${case_dir}/serial.log" \
        -device virtio-serial-pci,id=serial0 \
        -chardev "file,id=hvc,path=${case_dir}/hvc.log" \
        -device virtconsole,chardev=hvc \
        -drive "file=${case_dir}/disk.img,format=raw,if=none,id=disk" \
        -device virtio-blk-pci,drive=disk \
        -netdev user,id=net0,restrict=on,ipv4=on,net=10.115.0.0/24,host=10.115.0.1,dns=10.115.0.3,ipv6=on,ipv6-net=fd15:115::/64,ipv6-host=fd15:115::1 \
        -device virtio-net-pci,netdev=net0 \
        -object rng-random,id=rng0,filename=/dev/urandom \
        -device virtio-rng-pci,rng=rng0 \
        -fw_cfg "name=opt/gigahost/config,file=${case_dir}/config.json" \
        || rc=$?
    touch "${case_dir}/serial.log" "${case_dir}/hvc.log"
    if [[ ${rc} == 124 ]]; then
        fail "${name}: QEMU did not exit within ${case_timeout} s (no power-off)"
    elif [[ ${rc} != 0 ]]; then
        fail "${name}: QEMU exited with ${rc}"
    fi
}

# The one exited report on the serial console.
exited_report() {
    grep -a -o 'gigahost-init: report {"v":1,"event":"exited".*}' "${case_dir}/serial.log" | sed 's/^gigahost-init: report //' || true
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
    grep -a -q 'gigahost-init: report {"v":1,"event":"started","pid":[0-9]*,"monotonic_ms":[0-9]*}' "${case_dir}/serial.log" \
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

# 1. The kernel feature checks pass and the process exits 0.
before=${failures}
boot features '{"version":1,"argv":["/bin/sh","/usr/local/bin/features.sh"]}'
expect_started
passed=$(grep -a -c '^FEATURE [a-z0-9_]* PASS' "${case_dir}/hvc.log" || true)
[[ ${passed} == 21 ]] || fail "expected 21 passing feature checks, got ${passed}"
expect_absent hvc.log ' FAIL '
expect_report '"code":0,"signal":null,"reason":"exited"'
show_logs "${before}"

# 2. Exit codes and signals.
before=${failures}
boot exit-3 '{"version":1,"argv":["/bin/sh","-c","exit 3"]}'
expect_report '"code":3,"signal":null,"reason":"exited"'
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
boot cmdline-v2 '{"version":1,"argv":["/bin/sh","-c","echo ran"]}' "console=ttyS0 quiet gh.v=2 gh.hostname=case-host ${network}"
expect_report '"reason":"error","message":"gh.v names an interface version this init does not support"'
expect_absent serial.log '"event":"started"'
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

if [[ ${failures} != 0 ]]; then
    echo "boot.sh: ${failures} check(s) failed"
    exit 1
fi
echo "boot.sh: all cases passed"
