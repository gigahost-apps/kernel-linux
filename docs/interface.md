# The host ↔ init interface, version 1

This is the contract between the host that boots a container machine (the
agent) and `gigahost-init`, the init in this repository's initramfs. It is
written for agent authors: everything the agent writes and everything it
reads back is here.

A release's `build-info.json` lists the interface versions its init accepts
(`"interface_versions": [1]`). Any change to the kernel command line, the
config or the vsock lines below is a new interface version.

## The machine

The agent boots the release's `bzImage` and `initramfs.cpio.zst` by direct
kernel boot (QEMU `-kernel`, `-initrd`, `-append`, or libvirt's `<kernel>`,
`<initrd>`, `<cmdline>`) on the `pc` machine with:

- the renter's root disk as the first virtio-blk disk (`/dev/vda`), ext4;
- one virtio-net interface (`eth0`);
- a serial console (`ttyS0`) for the kernel and the init's own lines;
- a virtio console (`hvc0`) for the container's output, optional: without
  it the output goes to the serial console;
- a vsock device for the reports and the stop command, optional: without
  it the serial console carries the reports;
- the config as the fw_cfg entry `opt/gigahost/config`
  (`-fw_cfg name=opt/gigahost/config,file=...`);
- optionally virtio-rng;
- a reset that ends the domain: libvirt's `<on_reboot>destroy</on_reboot>`
  and `<on_crash>destroy</on_crash>`, or QEMU's `-no-reboot`.

The init powers the machine off when the container ends, so the agent sees
the domain stop by itself. The kernel resets the machine at once on a
panic (`CONFIG_PANIC_TIMEOUT=-1`), and the process, which runs as root
by default, can reboot or power the machine off itself; with the reset
ending the domain, each of these ends it too, without an `exited` report.
A halt by the process (`reboot(2)` with `RB_HALT_SYSTEM`) stops the
CPUs without ending the domain; the agent's stop ends it then (see
Trust). The domain's end is therefore the event the agent acts on; the
reports (below) say why it ended when the init could say so.

## Kernel command line

The agent writes it; no value contains a space:

```
console=ttyS0 quiet gh.v=1 gh.hostname=NAME gh.ip4=A.B.C.D/P gh.gw4=A.B.C.D
[gh.ip6=ADDR/P gh.gw6=ADDR] gh.dns=ADDR[,ADDR...] [gh.mtu=N] gh.vsock=PORT
```

| Key | Required | Value |
|---|---|---|
| `gh.v` | yes | The interface version, `1`. Any other value is refused. |
| `gh.hostname` | yes | The host name: dot-separated labels of letters, digits and hyphens (no label starting or ending with a hyphen), 64 bytes at most. |
| `gh.ip4` | yes | `eth0`'s IPv4 address and prefix length, `A.B.C.D/P` (P 0–32). |
| `gh.gw4` | yes | The IPv4 default gateway. A gateway outside `gh.ip4`'s prefix gets an on-link host route first. |
| `gh.ip6` | with `gh.gw6` | `eth0`'s IPv6 address and prefix length, `ADDR/P` (P 0–128). |
| `gh.gw6` | with `gh.ip6` | The IPv6 default gateway, link-local (`fe80::/10`) or global. |
| `gh.dns` | yes | One or more name servers, IPv4 or IPv6, comma-separated. |
| `gh.mtu` | no | `eth0`'s MTU, 68–65535. The interface's default when absent. |
| `gh.vsock` | yes | The vsock port of the reports and the control listener (not `4294967295`). |

Unknown `gh.*` keys are ignored; a key given twice is refused. Prefix
lengths are plain decimal (`/24`, not `/024`). When `gh.ip6` is given, the
init turns off duplicate address detection on `eth0`
(`accept_dad = 0`), so the platform's address is usable at once.

## Config

The fw_cfg entry `opt/gigahost/config`, read from
`/sys/firmware/qemu_fw_cfg/by_name/opt/gigahost/config/raw`: one JSON
object, at most 1 MiB (1,048,576 bytes).

```json
{ "version": 1,
  "argv": ["nginx", "-g", "daemon off;"],
  "env": ["KEY=value"],
  "user": "101:101",
  "workdir": "/",
  "stop_timeout_seconds": 10,
  "stop_signal": 15,
  "authorized_keys": { "user": "root", "keys": ["ssh-ed25519 ..."] } }
```

| Field | Required | Meaning |
|---|---|---|
| `version` | yes | `1`; any other value is refused. |
| `argv` | yes | The process and its arguments, not empty. `argv[0]` is searched in the process's `PATH` unless it holds a `/`. The image's entrypoint and command, with the renter's overrides, already combined by the agent. |
| `env` | no | `KEY=value` entries (a key is not empty). Default: none. |
| `user` | no | Docker's `user[:group]`, by name or number. Default: root (uid 0, gid 0). |
| `workdir` | no | An absolute path. Default: `/`. |
| `stop_timeout_seconds` | no | How long a stop waits for the process after the stop signal, 0–300. Default: 10. |
| `stop_signal` | no | The signal a stop sends the process first, as a number, 1–64: the image's `STOPSIGNAL` (for example 3 for `SIGQUIT`). Default: 15 (`SIGTERM`). |
| `authorized_keys` | no | SSH keys for the platform's stock images: `user` (a name, or a uid; a uid absent from the image's /etc/passwd has home `/`, so the keys go to `/.ssh/authorized_keys`) and `keys` (one key per entry, no line breaks). |

Unknown fields are refused, at the top level and inside `authorized_keys`.
No string may hold a NUL byte.

The config may carry secrets. The init never writes an `env` value, a key
or any other config value to the console or into a report; its error
messages name a field at most.

## What the init does

1. As PID 1 in the initramfs: mounts devtmpfs on `/dev`, opens
   `/dev/console` for its own output, logs `gigahost-init: start <ms>`
   (milliseconds of the guest's monotonic clock), mounts proc and sysfs.
2. Parses the kernel command line and reads the config; refuses an
   unknown interface or config version.
3. Waits up to 5 s for `/dev/vda` and mounts it (ext4) as the new root;
   moves `/dev`, `/proc` and `/sys` onto it; deletes the initramfs's files,
   so their memory is freed; moves the disk to `/` and chroots into it. The
   init stays PID 1; the disk carries none of its files.
4. Mounts, as Docker does: devpts on `/dev/pts` (`newinstance`,
   `ptmxmode=0666`, `mode=0620`, `gid=5`) with `/dev/pts/ptmx` bound over
   `/dev/ptmx`; tmpfs on `/dev/shm` (64 MiB, mode 1777); mqueue on
   `/dev/mqueue`; cgroup2 on `/sys/fs/cgroup` (`nsdelegate`); tmpfs on
   `/run`. `/tmp` stays as the image has it. Adds `/dev/fd`, `/dev/stdin`,
   `/dev/stdout` and `/dev/stderr`.
5. Sets the host name. Writes `hosts`, `resolv.conf` and `hostname` into
   `/run/gigahost/` and bind-mounts each over `/etc/hosts`,
   `/etc/resolv.conf` and `/etc/hostname`. The mount covers the path
   itself: when the image has a symbolic link there (Debian and Ubuntu
   point `/etc/resolv.conf` elsewhere), the link is covered, never followed,
   so nothing on the disk is written. The process sees and may change the
   init's copies; the disk's own files stay as they are.
6. Brings up `lo` and `eth0`: the addresses, the routes and the MTU from
   the command line.
7. With `authorized_keys`: writes the keys, one per line, to the user's
   `~/.ssh/authorized_keys` (directory 0700, file 0600, both owned by the
   user and the user's primary group), replacing the file.
8. Starts the process as Docker does:
   - every signal at its default action and none blocked;
   - stdin is `/dev/null`; stdout and stderr go to `/dev/hvc0` in raw mode
     (byte for byte, no `\r` added), or to the serial console without hvc0;
   - a new session, no controlling terminal;
   - the environment: `PATH`
     (`/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`)
     unless the config sets one, `HOSTNAME`, then the config's entries
     (each replacing an earlier entry of its key), then `HOME` from the
     user's passwd entry (or `/`) unless set;
   - the user resolved against the image's `/etc/passwd` and `/etc/group`
     as Docker's runtime resolves `user[:group]`: a name must be in passwd,
     a uid need not be (then gid 0, home `/`); a group by name must be in
     group, a gid need not be; without a group the user's passwd group is
     the primary group and the user's memberships in /etc/group are the
     supplementary groups;
   - the working directory, created (mode 0755, owned by root) when
     missing;
   - `argv[0]` searched in the environment's `PATH`.

   Then it reports `started`, and starts the control listener; an agent
   that sends `stop` as soon as it has read `started` retries a refused
   connection for a few seconds.
9. Reaps every process. When the main process ends, or on a stop command
   (below): SIGTERM to every other process, 2 s, SIGKILL to all; then the
   one `exited` report; sync; the root remounted read-only; power-off.

What the init may create on the disk, when the image lacks it: the
directories `/dev`, `/proc`, `/sys`, `/run` and `/etc`, empty
`/etc/hosts`, `/etc/resolv.conf` and `/etc/hostname` files (as mount
points), the working directory, and `~/.ssh/authorized_keys` for
`authorized_keys`. Nothing else on the disk is written but by the process.

Any failure in these steps (an unknown version, a bad config, no disk, a
user or `argv[0]` not found, a failed mount) ends in an `error` report and
a power-off: the init never exits, which would panic the kernel.

## Reports

For each event the init connects to vsock CID 2 (the host) on port
`gh.vsock`, writes one JSON line (ending in `\n`), shuts its side down and
closes once the host has closed or after 1 s. When the host does not
answer, it retries for 2 s; without a vsock device it does not try. Every
report is also printed on the serial console as

```
gigahost-init: report <json>
```

which is the agent's fallback when it missed the vsock message (parse the
serial log for lines starting with `gigahost-init: report `).

```json
{"v":1,"event":"started","pid":2,"monotonic_ms":871}
{"v":1,"event":"exited","code":3,"signal":null,"reason":"exited"}
{"v":1,"event":"exited","code":null,"signal":9,"reason":"exited"}
{"v":1,"event":"exited","code":null,"signal":15,"reason":"stopped"}
{"v":1,"event":"exited","code":null,"signal":null,"reason":"error","message":"..."}
```

- `started`: the process is running; `pid` is its PID in the guest,
  `monotonic_ms` the guest's monotonic clock at that moment.
- `exited`, at most once per boot, and the last report when it comes
  (a panic, or a reboot or power-off by the process, ends the machine
  without one):
  - `code`: the exit status when the process exited, else `null`;
  - `signal`: the signal that killed it, else `null`;
  - `reason`: `exited` (it ended by itself), `stopped` (it ended after a
    stop command, whatever its status) or `error` (the init could not
    start it; `code` and `signal` are `null`);
  - `message`: with `error` only, what went wrong, in words that quote no
    config value.

An `error` report can come without a `started` report before it.

## Control

Once it has reported `started`, the init listens on vsock port `gh.vsock`
of its own CID. The host connects and writes one line (at most 4096 bytes,
within 2 s):

```json
{"v":1,"command":"stop","timeout_seconds":20}
```

`timeout_seconds` is optional (the config's `stop_timeout_seconds` when
absent) and capped at 300. The init answers one line:

```json
{"v":1,"ok":true}
{"v":1,"ok":false,"error":"unknown command"}
```

- On `stop`: the config's `stop_signal` (SIGTERM by default) to the main
  process. When it has not ended after the timeout: SIGTERM to every
  process, 2 s, SIGKILL to every process. The end is then step 9; the
  `exited` report says `stopped`.
- A second stop, or a stop after the process ended, is answered `ok` and
  changes nothing. A stop racing the process's own exit still gives
  exactly one `exited` report.
- Errors: `unsupported interface version` (a `v` other than 1),
  `unknown command`, `invalid stop command` (an unknown field, a negative
  or non-numeric timeout), `the line is not a JSON object`,
  `the line is too long`.
- Connections from any CID but the host's (2) are closed unanswered.

This is the graceful stop: the kernel has no input layer, so it has no
ACPI power button. A forced stop is the hypervisor's (destroying the
domain).

## Trust

The machine is the renter's: the process runs as root by default, and
anything in the guest can speak the init's lines. The port `gh.vsock` is
in `/proc/cmdline`, so any guest process, root or not, can connect to the
host on it and write a report, or open many connections; a root process
can write `gigahost-init: report ...` lines to the serial console, or
take the control port before the init binds it. Reports are therefore
advisory, and the agent:

- bases billing and every state change on the domain's lifecycle (it
  started, it ended), never on a report;
- attributes a vsock connection to a machine by the peer's CID only,
  reads at most one line of a bounded length from it, caps the
  connections a machine may hold open, and keeps at most one `started`
  and one `exited` report per boot;
- from the serial log, takes the last `gigahost-init: report ` in the
  log and the JSON object after it on that line: the init kills every
  process before it writes its own `exited` report, so nothing in the
  guest writes after it;
- treats a stop as done only when the domain ends: when the init does not
  answer, or the domain has not ended within the stop's timeout plus a
  few seconds, it destroys the domain.
