//! Interface and route setup by ioctl, as `ifconfig` and `route` do, so
//! the image needs no `ip` command and the init no netlink code.

use super::{check, cstring, errno};
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const IFNAMSIZ: usize = 16;

#[repr(C)]
struct IfreqAddr {
    name: [u8; IFNAMSIZ],
    addr: libc::sockaddr_in,
    pad: [u8; 8],
}

#[repr(C)]
struct IfreqShort {
    name: [u8; IFNAMSIZ],
    value: libc::c_short,
    pad: [u8; 22],
}

#[repr(C)]
struct IfreqInt {
    name: [u8; IFNAMSIZ],
    value: libc::c_int,
    pad: [u8; 20],
}

/// `struct in6_ifreq` of `<linux/ipv6.h>`.
#[repr(C)]
struct In6Ifreq {
    addr: libc::in6_addr,
    prefixlen: u32,
    ifindex: libc::c_int,
}

/// `struct in6_rtmsg` of `<linux/ipv6_route.h>`.
#[repr(C)]
struct In6Rtmsg {
    dst: libc::in6_addr,
    src: libc::in6_addr,
    gateway: libc::in6_addr,
    kind: u32,
    dst_len: u16,
    src_len: u16,
    metric: u32,
    info: libc::c_ulong,
    flags: u32,
    ifindex: libc::c_int,
}

/// An interface name as the kernel wants it, or `InvalidInput`.
fn ifname(name: &str) -> io::Result<[u8; IFNAMSIZ]> {
    let mut out = [0u8; IFNAMSIZ];
    let target = out
        .get_mut(..name.len())
        .filter(|_| name.len() < IFNAMSIZ)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    target.copy_from_slice(name.as_bytes());
    Ok(out)
}

fn sockaddr(ip: Ipv4Addr) -> libc::sockaddr_in {
    libc::sockaddr_in {
        sin_family: libc::sa_family_t::try_from(libc::AF_INET).unwrap_or_default(),
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(ip.octets()),
        },
        sin_zero: [0; 8],
    }
}

fn socket(family: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: socket has no memory preconditions.
    let fd = check(unsafe { libc::socket(family, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: `fd` was just returned to us and is owned by nobody else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// The request constants are `c_ulong`; glibc's ioctl takes that, musl's
// an int (`libc::Ioctl` either way). The values fit both.
#[allow(
    clippy::useless_conversion,
    clippy::unnecessary_fallible_conversions,
    reason = "a no-op on glibc, a narrowing on musl"
)]
fn ioctl<T>(fd: &OwnedFd, request: libc::c_ulong, argument: &mut T) -> io::Result<()> {
    let request =
        libc::Ioctl::try_from(request).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `argument` is a live, writable value of the repr(C) type the
    // request expects (each caller pairs the request with its struct).
    let rc = unsafe { libc::ioctl(fd.as_raw_fd(), request, std::ptr::from_mut(argument)) };
    if rc < 0 { Err(errno()) } else { Ok(()) }
}

/// Sets `IFF_UP` on the interface.
pub fn set_up(name: &str) -> io::Result<()> {
    let fd = socket(libc::AF_INET)?;
    let mut request = IfreqShort {
        name: ifname(name)?,
        value: 0,
        pad: [0; 22],
    };
    ioctl(&fd, libc::SIOCGIFFLAGS, &mut request)?;
    request.value |= libc::c_short::try_from(libc::IFF_UP).unwrap_or_default();
    ioctl(&fd, libc::SIOCSIFFLAGS, &mut request)
}

/// Sets the interface's MTU.
pub fn set_mtu(name: &str, mtu: u32) -> io::Result<()> {
    let fd = socket(libc::AF_INET)?;
    let mut request = IfreqInt {
        name: ifname(name)?,
        value: libc::c_int::try_from(mtu)
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?,
        pad: [0; 20],
    };
    ioctl(&fd, libc::SIOCSIFMTU, &mut request)
}

/// The interface's index.
pub fn index(name: &str) -> io::Result<libc::c_int> {
    let fd = socket(libc::AF_INET)?;
    let mut request = IfreqInt {
        name: ifname(name)?,
        value: 0,
        pad: [0; 20],
    };
    ioctl(&fd, libc::SIOCGIFINDEX, &mut request)?;
    Ok(request.value)
}

fn ipv4_mask(prefix: u8) -> Ipv4Addr {
    Ipv4Addr::from(
        u32::MAX
            .checked_shl(32 - u32::from(prefix.min(32)))
            .unwrap_or(0),
    )
}

/// Gives the interface an IPv4 address and netmask (which also adds the
/// prefix's on-link route).
pub fn add_ipv4(name: &str, address: Ipv4Addr, prefix: u8) -> io::Result<()> {
    let fd = socket(libc::AF_INET)?;
    let mut request = IfreqAddr {
        name: ifname(name)?,
        addr: sockaddr(address),
        pad: [0; 8],
    };
    ioctl(&fd, libc::SIOCSIFADDR, &mut request)?;
    request.addr = sockaddr(ipv4_mask(prefix));
    ioctl(&fd, libc::SIOCSIFNETMASK, &mut request)
}

/// Adds an IPv4 route on `device` to `destination/prefix`, through
/// `gateway` or on-link.
pub fn add_ipv4_route(
    device: &str,
    destination: Ipv4Addr,
    prefix: u8,
    gateway: Option<Ipv4Addr>,
) -> io::Result<()> {
    let fd = socket(libc::AF_INET)?;
    let device = cstring(device)?;
    // SAFETY: rtentry is plain data for which all zeroes is valid.
    let mut route: libc::rtentry = unsafe { std::mem::zeroed() };
    write_sockaddr(&mut route.rt_dst, sockaddr(destination));
    write_sockaddr(&mut route.rt_genmask, sockaddr(ipv4_mask(prefix)));
    let mut flags = libc::RTF_UP;
    if prefix == 32 {
        flags |= libc::RTF_HOST;
    }
    if let Some(gateway) = gateway {
        write_sockaddr(&mut route.rt_gateway, sockaddr(gateway));
        flags |= libc::RTF_GATEWAY;
    }
    route.rt_flags = flags;
    // The kernel only reads the name; it outlives the call.
    route.rt_dev = device.as_ptr().cast_mut();
    ioctl(&fd, libc::SIOCADDRT, &mut route)
}

fn write_sockaddr(target: &mut libc::sockaddr, value: libc::sockaddr_in) {
    // SAFETY: sockaddr and sockaddr_in have the same size (16 bytes) and
    // sockaddr_in is plain data; the write is unaligned-safe.
    unsafe {
        std::ptr::from_mut(target)
            .cast::<libc::sockaddr_in>()
            .write_unaligned(value);
    };
}

/// Gives the interface an IPv6 address.
pub fn add_ipv6(ifindex: libc::c_int, address: Ipv6Addr, prefix: u8) -> io::Result<()> {
    let fd = socket(libc::AF_INET6)?;
    let mut request = In6Ifreq {
        addr: libc::in6_addr {
            s6_addr: address.octets(),
        },
        prefixlen: u32::from(prefix),
        ifindex,
    };
    ioctl(&fd, libc::SIOCSIFADDR, &mut request)
}

/// Adds an IPv6 route on the interface to `destination/prefix`, through
/// `gateway` or on-link.
pub fn add_ipv6_route(
    ifindex: libc::c_int,
    destination: Ipv6Addr,
    prefix: u8,
    gateway: Option<Ipv6Addr>,
) -> io::Result<()> {
    let fd = socket(libc::AF_INET6)?;
    let mut flags = u32::from(libc::RTF_UP);
    if prefix == 128 {
        flags |= u32::from(libc::RTF_HOST);
    }
    if gateway.is_some() {
        flags |= u32::from(libc::RTF_GATEWAY);
    }
    let mut route = In6Rtmsg {
        dst: libc::in6_addr {
            s6_addr: destination.octets(),
        },
        src: libc::in6_addr { s6_addr: [0; 16] },
        gateway: libc::in6_addr {
            s6_addr: gateway.unwrap_or(Ipv6Addr::UNSPECIFIED).octets(),
        },
        kind: 0,
        dst_len: u16::from(prefix),
        src_len: 0,
        metric: 1,
        info: 0,
        flags,
        ifindex,
    };
    ioctl(&fd, libc::SIOCADDRT, &mut route)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks() {
        assert_eq!(ipv4_mask(0), Ipv4Addr::UNSPECIFIED);
        assert_eq!(ipv4_mask(24), Ipv4Addr::new(255, 255, 255, 0));
        assert_eq!(ipv4_mask(32), Ipv4Addr::BROADCAST);
    }

    #[test]
    fn names() {
        assert!(ifname("eth0").is_ok());
        assert!(ifname("a-name-too-long-x").is_err());
    }

    #[test]
    fn layouts_match_the_kernel() {
        assert_eq!(std::mem::size_of::<IfreqAddr>(), 40);
        assert_eq!(std::mem::size_of::<IfreqShort>(), 40);
        assert_eq!(std::mem::size_of::<IfreqInt>(), 40);
        assert_eq!(std::mem::size_of::<In6Ifreq>(), 24);
        assert_eq!(std::mem::size_of::<In6Rtmsg>(), 80);
    }
}
