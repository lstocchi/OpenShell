// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! System DNS resolver discovery for Windows hosts.
//!
//! The supervisor's policy DNS forwards guest queries to the host's trusted
//! resolver; Unix reads the first `nameserver` from `/etc/resolv.conf`.
//! Windows has no such file, so the first DNS server of the adapter that
//! carries the default route is used instead, via `GetAdaptersAddresses`.

#![allow(unsafe_code)]

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, GetAdaptersAddresses,
    GetBestInterfaceEx, IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::NET_IF_OPER_STATUS_UP;
use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, AF_UNSPEC, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKET_ADDRESS,
};

/// The first usable DNS server of the adapter carrying the default route,
/// mirroring the first `nameserver` entry of `/etc/resolv.conf` on Unix.
///
/// Enumeration order alone is not routing priority: Hyper-V, WSL, Docker,
/// and VPN adapters are often listed first while the default route uses
/// another adapter. Adapters are therefore ranked by whether they carry the
/// default route, then by interface metric.
pub fn system_resolver() -> io::Result<IpAddr> {
    let buffer = adapter_addresses()?;
    let default_v4 = best_interface(SocketAddr::from((Ipv4Addr::new(8, 8, 8, 8), 53)));
    let default_v6 = best_interface(SocketAddr::from((
        Ipv6Addr::new(0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888),
        53,
    )));

    let mut best: Option<((bool, u32, usize), IpAddr)> = None;
    // SAFETY: `GetAdaptersAddresses` succeeded with this buffer, so the head
    // pointer and the whole `Next` chain it references are initialized.
    unsafe {
        let mut adapter = if buffer.is_empty() {
            std::ptr::null()
        } else {
            buffer.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()
        };
        let mut position = 0;
        while !adapter.is_null() {
            let current = &*adapter;
            adapter = current.Next;
            position += 1;
            if current.OperStatus != NET_IF_OPER_STATUS_UP
                || current.IfType == IF_TYPE_SOFTWARE_LOOPBACK
            {
                continue;
            }
            let Some(resolver) = first_usable_server(current) else {
                continue;
            };
            let if_index = current.Anonymous1.Anonymous.IfIndex;
            let carries_default = (default_v4.is_some() && default_v4 == Some(if_index))
                || (default_v6.is_some() && default_v6 == Some(current.Ipv6IfIndex));
            let metric = current.Ipv4Metric.min(current.Ipv6Metric);
            // Lower ranks win: default route first, then metric, then
            // enumeration order.
            let rank = (!carries_default, metric, position);
            if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
                best = Some((rank, resolver));
            }
        }
    }
    best.map(|(_, resolver)| resolver).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no operational adapter has a configured DNS server",
        )
    })
}

/// The first configured DNS server of `adapter` that can be dialed.
///
/// # Safety
///
/// `adapter` must come from a successful `GetAdaptersAddresses` call.
unsafe fn first_usable_server(adapter: &IP_ADAPTER_ADDRESSES_LH) -> Option<IpAddr> {
    let mut server = adapter.FirstDnsServerAddress;
    while !server.is_null() {
        // SAFETY: The caller guarantees the server list is initialized.
        let (address, next) = unsafe { (&(*server).Address, (*server).Next) };
        // SAFETY: Same guarantee for the socket address it references.
        if let Some(resolver) = unsafe { sockaddr_ip(address) }
            && is_usable_resolver(resolver)
        {
            return Some(resolver);
        }
        server = next;
    }
    None
}

/// Windows reports `fec0:0:0:ffff::1` through `::3` as DNS servers on
/// adapters without a real one (virtualization stacks do this); they are
/// site-local placeholders that would silently drop forwarded queries.
/// Link-local IPv6 servers need a scope id that an `IpAddr` cannot carry.
fn is_usable_resolver(ip: IpAddr) -> bool {
    let IpAddr::V6(v6) = ip else {
        return true;
    };
    v6.segments()[..4] != [0xfec0, 0, 0, 0xffff] && !v6.is_unicast_link_local()
}

/// The interface index the routing table picks for `destination`, or `None`
/// when no route exists for that family. Only the route table is consulted;
/// nothing is sent.
fn best_interface(destination: SocketAddr) -> Option<u32> {
    let address = socket2::SockAddr::from(destination);
    let mut index = 0_u32;
    // SAFETY: The address points at a valid sockaddr for its family and the
    // output is a plain integer.
    let result = unsafe { GetBestInterfaceEx(address.as_ptr().cast::<SOCKADDR>(), &raw mut index) };
    (result == NO_ERROR).then_some(index)
}

/// SAFETY contract: the returned buffer holds a fully initialized
/// `IP_ADAPTER_ADDRESSES_LH` list, or is empty when the host has no adapters.
///
/// The buffer is made of `u64` words so the structs it holds are aligned.
fn adapter_addresses() -> io::Result<Vec<u64>> {
    const WORD: usize = size_of::<u64>();
    let mut buffer: Vec<u64> = Vec::new();
    let mut size: u32 = 0;
    for _ in 0..4 {
        let pointer = if buffer.capacity() == 0 {
            std::ptr::null_mut()
        } else {
            buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()
        };
        // SAFETY: The sizing call passes a null buffer with a zero length;
        // the retry passes a buffer reserved for at least the reported
        // length, which `GetAdaptersAddresses` initializes on success.
        let result = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_UNSPEC),
                GAA_FLAG_SKIP_UNICAST | GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST,
                std::ptr::null(),
                pointer,
                &raw mut size,
            )
        };
        match result {
            NO_ERROR => {
                // SAFETY: The call initialized `size` bytes of the reserved
                // capacity; only whole words are exposed.
                unsafe { buffer.set_len(buffer.capacity().min(size as usize / WORD)) };
                return Ok(buffer);
            }
            ERROR_NO_DATA => return Ok(Vec::new()),
            ERROR_BUFFER_OVERFLOW => {
                buffer = Vec::with_capacity((size as usize).div_ceil(WORD));
                size = u32::try_from(buffer.capacity() * WORD).unwrap_or(u32::MAX);
            }
            // The IP Helper API returns its error code directly instead of
            // setting the thread's last error.
            error => return Err(io::Error::from_raw_os_error(error.cast_signed())),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::OutOfMemory,
        "GetAdaptersAddresses kept reporting a larger buffer",
    ))
}

/// Parse the IP address of a `SOCKET_ADDRESS` returned by the adapter API.
///
/// # Safety
///
/// `address` must point at a socket address initialized by a successful
/// `GetAdaptersAddresses` call.
unsafe fn sockaddr_ip(address: &SOCKET_ADDRESS) -> Option<IpAddr> {
    let sockaddr = address.lpSockaddr.cast_const();
    let length = usize::try_from(address.iSockaddrLength).ok()?;
    if sockaddr.is_null() || length < size_of::<SOCKADDR>() {
        return None;
    }
    // SAFETY: The caller guarantees the sockaddr is initialized, and every
    // dereference is guarded by the per-family length check.
    unsafe {
        match (*sockaddr).sa_family {
            AF_INET if length >= size_of::<SOCKADDR_IN>() => {
                let v4 = sockaddr.cast::<SOCKADDR_IN>().read_unaligned();
                let octets = v4.sin_addr.S_un.S_un_b;
                Some(IpAddr::from([
                    octets.s_b1,
                    octets.s_b2,
                    octets.s_b3,
                    octets.s_b4,
                ]))
            }
            AF_INET6 if length >= size_of::<SOCKADDR_IN6>() => {
                let v6 = sockaddr.cast::<SOCKADDR_IN6>().read_unaligned();
                Some(IpAddr::from(v6.sin6_addr.u.Byte))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_and_link_local_resolvers_are_unusable() {
        for unusable in ["fec0:0:0:ffff::1", "fec0:0:0:ffff::3", "fe80::1"] {
            assert!(!is_usable_resolver(unusable.parse().unwrap()), "{unusable}");
        }
        for usable in ["10.0.0.1", "127.0.0.1", "2001:4860:4860::8888", "fd00::53"] {
            assert!(is_usable_resolver(usable.parse().unwrap()), "{usable}");
        }
    }

    #[test]
    #[ignore = "requires a host with a configured DNS server"]
    fn system_resolver_finds_a_host_dns_server() {
        let resolver = system_resolver().expect("host DNS resolver");
        assert!(is_usable_resolver(resolver), "{resolver}");
        println!("host resolver: {resolver}");
    }
}
