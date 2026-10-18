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
    echo "INFO pid1_cgroup $(cat /proc/1/cgroup)"
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
    check cron pidof crond
fi
check cgroup_root grep -q -x '0::/.*' /proc/self/cgroup
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
