#!/bin/sh
# The checks of tests/boot.sh's init-system cases, run by the image's init
# system once it has booted (systemd or openrc, the first argument): FEATURE
# lines and INFO lines on the console, then the container powers itself off,
# or reboots when the host name starts with "reboot-".
set -u
kind=$1
exec > /dev/console 2>&1

result() {
    echo "FEATURE $1 $2"
}

check() {
    name=$1
    shift
    if "$@" > /dev/null 2>&1; then
        result "${name}" PASS
    else
        result "${name}" FAIL
    fi
}

# wait_for SECONDS COMMAND...: true once COMMAND is, polling.
wait_for() {
    tries=$(($1 * 5))
    shift
    while [ "${tries}" -gt 0 ]; do
        "$@" > /dev/null 2>&1 && return 0
        tries=$((tries - 1))
        sleep 0.2
    done
    return 1
}

# The cgroup namespace's root is the cgroup the init made for the image
# (payload): seen from inside, no cgroup path names it, and the cgroup2 on
# /sys/fs/cgroup shows that root, so the script's own cgroup is in it at
# the path /proc/self/cgroup gives. Without the cgroup namespace the paths
# start with /payload; with the machine's cgroup2 left on /sys/fs/cgroup,
# payload is a directory there.
cgroup_root() {
    grep -q -x '0::/.*' /proc/self/cgroup && ! grep -q payload /proc/self/cgroup
}

cgroup_mount() {
    ! test -e /sys/fs/cgroup/payload \
        && test -d "/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)"
}

openrc_settled() {
    [ "$(cat /run/openrc/softlevel 2> /dev/null)" = default ] \
        && ! rc-status --all 2> /dev/null | grep -q -E 'starting|stopping'
}

if [ "${kind}" = systemd ]; then
    state=$(systemctl is-system-running --wait 2> /dev/null)
    echo "INFO boot_seconds $(cut -d' ' -f1 /proc/uptime)"
    echo "INFO state ${state}"
    systemctl --failed --no-legend --plain 2> /dev/null | sed 's/^/INFO failed /'
    check running test "${state}" = running
    check pid1 test "$(cat /proc/1/comm)" = systemd
    # systemd at the root of its cgroup namespace moves itself to
    # init.scope, as under any container manager.
    check pid1_cgroup test "$(cat /proc/1/cgroup)" = 0::/init.scope
    echo "INFO virtualization $(systemd-detect-virt 2> /dev/null)"
    marker=init-features-$$
    echo "${marker}" | systemd-cat -t init-features
    check journal wait_for 10 sh -c "journalctl -q -t init-features | grep -q ${marker}"
    check cron wait_for 10 test -e /run/cron-ran
else
    wait_for 20 openrc_settled
    echo "INFO boot_seconds $(cut -d' ' -f1 /proc/uptime)"
    echo "INFO rc_sys $(openrc --sys 2> /dev/null)"
    rc-status --all 2> /dev/null | sed 's/^/INFO rc-status /'
    check running openrc_settled
    check pid1 test "$(readlink /proc/1/exe)" = /bin/busybox
    # busybox init stays where it started: the cgroup namespace's root.
    check pid1_cgroup test "$(cat /proc/1/cgroup)" = 0::/
    check cron pidof crond
fi
echo "INFO pid1_cgroup $(cat /proc/1/cgroup)"
echo "INFO own_cgroup $(cat /proc/self/cgroup)"
check cgroup_root cgroup_root
check cgroup_mount cgroup_mount
check cgroup_writable sh -c 'mkdir /sys/fs/cgroup/init-features && rmdir /sys/fs/cgroup/init-features'
check sys_readonly sh -c 'grep " /sys sysfs " /proc/mounts | grep -q "[ ,]ro[ ,]"'
check no_init_of_the_machine sh -c '! grep -q -s gigahost-init /proc/*/comm'
check address4 sh -c 'ip -4 addr show eth0 | grep -q "inet 10.115.0.15/24"'
check address6 sh -c 'ip -6 addr show eth0 | grep -q "inet6 fd15:115::15/64"'
echo "INFO $(grep MemAvailable /proc/meminfo | tr -s ' ')"
case $(hostname) in
    reboot-*) action=reboot ;;
    *) action=poweroff ;;
esac
echo "INFO action ${action}"
sync
if [ "${kind}" = systemd ]; then
    systemctl "${action}"
else
    "${action}"
fi
