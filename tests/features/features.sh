#!/bin/sh
# Kernel feature checks, run as the container's process by tests/boot.sh.
# One line per check, "FEATURE <name> PASS|FAIL <detail>"; exits 1 when a
# check fails. The addresses are those of the boot test's user network.
failures=0
r() {
    name=$1
    shift
    out=$("$@" 2>&1)
    rc=$?
    out=$(echo "${out}" | tr '\n' ' ' | cut -c1-400)
    if [ "${rc}" = 0 ]; then
        echo "FEATURE ${name} PASS ${out}"
    else
        echo "FEATURE ${name} FAIL rc=${rc} ${out}"
        failures=$((failures + 1))
    fi
}
# The kernel's link-local address can still be in duplicate address
# detection right after boot; wait up to 3 s for it to settle.
settle_ipv6() {
    i=0
    while [ "${i}" -lt 30 ] && ip -6 addr show dev eth0 | grep -q tentative; do
        sleep 0.1
        i=$((i + 1))
    done
}
r userns_unshare unshare -U -n -m -p -f true
r userns_root_map unshare -U -r id -u
r seccomp_status_line sh -c 'grep Seccomp /proc/self/status'
r seccomp_filter /usr/local/bin/seccomp-test
controllers() {
    c=$(cat /sys/fs/cgroup/cgroup.controllers)
    echo "${c}"
    for x in cpuset cpu io memory hugetlb pids rdma misc; do
        echo " ${c} " | grep -q " ${x} " || return 1
    done
}
r cgroup2_controllers controllers
# The process's cgroup namespace root holds its processes, so, as in a
# Docker container (docker-in-docker's entrypoint does the same), they
# move into a child cgroup before the root's controllers are delegated.
cgroup_child() {
    mkdir -p /sys/fs/cgroup/init
    while read -r pid; do
        echo "${pid}" > /sys/fs/cgroup/init/cgroup.procs 2> /dev/null || true
    done < /sys/fs/cgroup/cgroup.procs
    echo "+memory +pids +cpu +io" > /sys/fs/cgroup/cgroup.subtree_control &&
        mkdir -p /sys/fs/cgroup/t &&
        echo 64M > /sys/fs/cgroup/t/memory.max &&
        cat /sys/fs/cgroup/t/memory.max &&
        rmdir /sys/fs/cgroup/t
}
r cgroup2_child cgroup_child
r overlayfs sh -c 'mkdir -p /tmp/o/l /tmp/o/u /tmp/o/w /tmp/o/m && echo lower > /tmp/o/l/f && mount -t overlay overlay -o lowerdir=/tmp/o/l,upperdir=/tmp/o/u,workdir=/tmp/o/w /tmp/o/m && cat /tmp/o/m/f && echo up > /tmp/o/m/g && ls /tmp/o/u && umount /tmp/o/m'
r dev_fuse sh -c 'test -c /dev/fuse && ls -l /dev/fuse'
r wireguard sh -c 'ip link add wg0 type wireguard && ip -d link show wg0 | head -3 && ip link del wg0'
r nft_nat_masquerade sh -c 'nft add table ip nat && nft add chain ip nat postrouting "{ type nat hook postrouting priority 100; }" && nft add rule ip nat postrouting oifname eth0 masquerade && nft add table inet filter && nft add chain inet filter input "{ type filter hook input priority 0; }" && nft add rule inet filter input ct state established,related accept && nft list ruleset && nft flush ruleset'
r iptables_nft_nat_list iptables-nft -t nat -L -n
r iptables_nft_rules sh -c 'iptables-nft -t nat -A POSTROUTING -o eth0 -j MASQUERADE && iptables-nft -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT && iptables-nft -A INPUT -p tcp -m multiport --dports 22,80 -m comment --comment check -j ACCEPT && iptables-nft -A FORWARD -i wg+ -j ACCEPT && iptables-nft -t mangle -A FORWARD -p tcp --tcp-flags SYN,RST SYN -j TCPMSS --clamp-mss-to-pmtu && iptables-nft -t nat -A PREROUTING -p udp --dport 51820 -j DNAT --to-destination 10.0.0.2 && ip6tables-nft -t nat -A POSTROUTING -o eth0 -j MASQUERADE && iptables-nft-save | grep -c -- "-A" && iptables-nft -t nat -F && iptables-nft -F && ip6tables-nft -t nat -F'
settle_ipv6
r ipv6_eth0 sh -c 'ip -6 addr show dev eth0 | grep -E "inet6 fd15:115::15" && ! ip -6 addr show dev eth0 | grep -q tentative'
r ipv6_link_local_ping sh -c 'ping -6 -c 1 -W 2 ff02::1%eth0 | head -3'
r ipv6_gateway_ping ping -6 -c 1 -W 2 fd15:115::1
r ipv4_gateway_ping ping -c 1 -W 2 10.115.0.1
r dev_kvm_absent sh -c '! test -e /dev/kvm'
r tun sh -c 'test -c /dev/net/tun || { mkdir -p /dev/net; mknod /dev/net/tun c 10 200; }; ip tuntap add dev tun0 mode tun && ip link del tun0'
r veth_bridge sh -c 'ip link add v0 type veth peer name v1 && ip link add br0 type bridge && ip link set v0 master br0 && ip link del br0 && ip link del v0'
r modules_off sh -c '! test -e /proc/modules'
r kernel_version sh -c 'uname -r | grep -x "[0-9.]*-gigahost-container"'
echo "FEATURES failed=${failures}"
[ "${failures}" = 0 ]
