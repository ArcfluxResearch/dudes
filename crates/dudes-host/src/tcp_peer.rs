//! Which process owns the client end of a loopback TCP connection, and who its parents are.
//!
//! The Floor's HTTP listener uses this to refuse a request that comes from an agent: the
//! connection has no credential the daemon can trust to name its sender, but the kernel knows which
//! process holds the client socket. Linux reads `/proc/net/tcp{,6}` for the socket's inode and then
//! `/proc/<pid>/fd` for its holder. macOS walks the user's own processes with `libproc`.
//!
//! No `unsafe` here. The macOS socket record is read through `libproc`'s safe `pidfdinfo`, asked
//! for a plain byte buffer instead of its typed layout, because the typed layout puts the ports in
//! a union that only unsafe code can read. The offsets into those bytes are pinned by a test
//! against that typed layout.
//!
//! Every answer here is a fact about one instant. A client that exits before the lookup is not
//! found, and callers must treat "not found" as a refusal.

use std::net::SocketAddrV4;

/// A process and the process group it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessRef {
    /// The process id.
    pub pid: u32,
    /// The process group id.
    pub group: u32,
}

/// The process that holds a socket, and its parents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SocketOwner {
    /// The user the owner runs as.
    pub uid: u32,
    /// The owner first, then its parent, then that parent's parent, up to the first process the
    /// walk cannot read or [`MAX_LINEAGE`] hops.
    pub lineage: Vec<ProcessRef>,
}

/// What the lookup found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketOwnerLookup {
    /// A process holds the socket.
    Found(SocketOwner),
    /// No process this daemon may inspect holds it, or the holder left before it could be read.
    NotFound,
    /// This platform cannot answer.
    Unsupported,
}

/// The most parents followed from the owner.
pub const MAX_LINEAGE: usize = 64;

/// Find the holder of the client end of a loopback connection.
///
/// `client` is the address the connecting peer's socket is bound to and `server` is the listener's
/// address, both as the accepting side reports them.
#[must_use]
pub fn loopback_socket_owner(client: SocketAddrV4, server: SocketAddrV4) -> SocketOwnerLookup {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::owner_pid(client, server)
            .and_then(lineage)
            .map_or(SocketOwnerLookup::NotFound, SocketOwnerLookup::Found)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (client, server);
        SocketOwnerLookup::Unsupported
    }
}

/// The process group of a live process, or `None` when it is gone or unreadable.
#[must_use]
pub fn process_group(pid: u32) -> Option<u32> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::row(pid).map(|row| row.group)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// The user this process runs as, where the platform can say.
#[must_use]
#[cfg_attr(
    unix,
    expect(
        clippy::unnecessary_wraps,
        reason = "one signature for platforms that cannot say"
    )
)]
pub fn current_uid() -> Option<u32> {
    #[cfg(unix)]
    {
        Some(rustix::process::getuid().as_raw())
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Row {
    parent: u32,
    group: u32,
    uid: u32,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn lineage(owner: u32) -> Option<SocketOwner> {
    let first = platform::row(owner)?;
    let mut chain = vec![ProcessRef {
        pid: owner,
        group: first.group,
    }];
    let mut parent = first.parent;
    while parent != 0 && chain.len() < MAX_LINEAGE && chain.iter().all(|seen| seen.pid != parent) {
        let Some(row) = platform::row(parent) else {
            break;
        };
        chain.push(ProcessRef {
            pid: parent,
            group: row.group,
        });
        parent = row.parent;
    }
    Some(SocketOwner {
        uid: first.uid,
        lineage: chain,
    })
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;
    use std::net::SocketAddrV4;
    use std::os::unix::fs::MetadataExt;

    use super::Row;

    pub(super) fn row(pid: u32) -> Option<Row> {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name is parenthesised and may itself hold spaces and parentheses.
        let after = stat.get(stat.rfind(')')? + 1..)?;
        let mut fields = after.split_whitespace().skip(1);
        let parent = fields.next()?.parse().ok()?;
        let group = fields.next()?.parse().ok()?;
        let uid = fs::metadata(format!("/proc/{pid}")).ok()?.uid();
        Some(Row { parent, group, uid })
    }

    pub(super) fn owner_pid(client: SocketAddrV4, server: SocketAddrV4) -> Option<u32> {
        let inode =
            inode_of("/proc/net/tcp", &v4_field(client), &v4_field(server)).or_else(|| {
                inode_of(
                    "/proc/net/tcp6",
                    &mapped_field(client),
                    &mapped_field(server),
                )
            })?;
        let wanted = format!("socket:[{inode}]");
        for entry in fs::read_dir("/proc").ok()?.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
                continue;
            };
            let holds = fds.flatten().any(|fd| {
                fs::read_link(fd.path()).is_ok_and(|target| target.as_os_str() == wanted.as_str())
            });
            if holds {
                return Some(pid);
            }
        }
        None
    }

    /// `/proc/net/tcp` prints an address as the host-order bytes of the `u32`, in hex.
    fn v4_field(addr: SocketAddrV4) -> String {
        format!(
            "{:08X}:{:04X}",
            u32::from_ne_bytes(addr.ip().octets()),
            addr.port()
        )
    }

    /// `/proc/net/tcp6` prints four such words, and a v4 peer on a v6 socket is `::ffff:a.b.c.d`.
    fn mapped_field(addr: SocketAddrV4) -> String {
        format!(
            "0000000000000000{:08X}{:08X}:{:04X}",
            u32::from_ne_bytes([0, 0, 0xFF, 0xFF]),
            u32::from_ne_bytes(addr.ip().octets()),
            addr.port()
        )
    }

    fn inode_of(file: &str, local: &str, remote: &str) -> Option<u64> {
        let table = fs::read_to_string(file).ok()?;
        table.lines().skip(1).find_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || !fields[1].eq_ignore_ascii_case(local) {
                return None;
            }
            if !fields[2].eq_ignore_ascii_case(remote) {
                return None;
            }
            fields[9].parse().ok().filter(|inode| *inode != 0)
        })
    }

    #[cfg(test)]
    pub(super) fn fields_for_test(addr: SocketAddrV4) -> (String, String) {
        (v4_field(addr), mapped_field(addr))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use libproc::bsd_info::BSDInfo;
    use libproc::file_info::{ListFDs, PIDFDInfo, PIDFDInfoFlavor, ProcFDType, pidfdinfo};
    use libproc::proc_pid::{listpidinfo, pidinfo};
    use libproc::processes::{ProcFilter, pids_by_type};

    use super::Row;

    pub(super) const SOCKET_INFO_LEN: usize = 792;
    pub(super) const KIND_AT: usize = 256;
    pub(super) const TCP_KIND: i32 = 2;
    pub(super) const FOREIGN_PORT_AT: usize = 264;
    pub(super) const LOCAL_PORT_AT: usize = 268;
    pub(super) const VERSION_FLAGS_AT: usize = 288;
    pub(super) const FOREIGN_V4_AT: usize = 296 + 12;
    pub(super) const LOCAL_V4_AT: usize = 312 + 12;
    const IPV4_FLAG: u8 = 1;

    /// The kernel's `socket_fdinfo`, as bytes.
    struct SocketBytes([u8; SOCKET_INFO_LEN]);

    impl Default for SocketBytes {
        fn default() -> Self {
            Self([0; SOCKET_INFO_LEN])
        }
    }

    impl PIDFDInfo for SocketBytes {
        fn flavor() -> PIDFDInfoFlavor {
            PIDFDInfoFlavor::SocketInfo
        }
    }

    pub(super) fn row(pid: u32) -> Option<Row> {
        let info = pidinfo::<BSDInfo>(i32::try_from(pid).ok()?, 0).ok()?;
        Some(Row {
            parent: info.pbi_ppid,
            group: info.pbi_pgid,
            uid: info.pbi_uid,
        })
    }

    fn word(bytes: &[u8; SOCKET_INFO_LEN], at: usize) -> i32 {
        i32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    }

    /// Whether these bytes are a TCP socket from `client` to `server`, both on IPv4.
    pub(super) fn is_connection(
        bytes: &[u8; SOCKET_INFO_LEN],
        client: SocketAddrV4,
        server: SocketAddrV4,
    ) -> bool {
        if word(bytes, KIND_AT) != TCP_KIND || bytes[VERSION_FLAGS_AT] & IPV4_FLAG == 0 {
            return false;
        }
        // The port is the low half of an `int`, in network byte order; macOS is little-endian.
        let port = |at: usize| u16::from_be(u16::from_ne_bytes([bytes[at], bytes[at + 1]]));
        let address = |at| Ipv4Addr::new(bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]);
        port(LOCAL_PORT_AT) == client.port()
            && port(FOREIGN_PORT_AT) == server.port()
            && address(LOCAL_V4_AT) == *client.ip()
            && address(FOREIGN_V4_AT) == *server.ip()
    }

    pub(super) fn owner_pid(client: SocketAddrV4, server: SocketAddrV4) -> Option<u32> {
        let uid = super::current_uid()?;
        let pids = pids_by_type(ProcFilter::ByUID { uid }).ok()?;
        for pid in pids.into_iter().filter(|pid| *pid != 0) {
            let Ok(raw) = i32::try_from(pid) else {
                continue;
            };
            let Ok(info) = pidinfo::<BSDInfo>(raw, 0) else {
                continue;
            };
            let Ok(fds) = listpidinfo::<ListFDs>(raw, info.pbi_nfiles as usize) else {
                continue;
            };
            for fd in fds {
                if !matches!(ProcFDType::from(fd.proc_fdtype), ProcFDType::Socket) {
                    continue;
                }
                if let Ok(SocketBytes(bytes)) = pidfdinfo::<SocketBytes>(raw, fd.proc_fd)
                    && is_connection(&bytes, client, server)
                {
                    return Some(pid);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    use std::net::{SocketAddr, TcpListener, TcpStream};

    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn v4(addr: SocketAddr) -> SocketAddrV4 {
        match addr {
            SocketAddr::V4(addr) => addr,
            SocketAddr::V6(_) => panic!("the test binds IPv4"),
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_socket_this_process_holds_is_found_with_its_parents() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server_side, _) = listener.accept().unwrap();
        let found = loopback_socket_owner(
            v4(client.local_addr().unwrap()),
            v4(listener.local_addr().unwrap()),
        );
        let SocketOwnerLookup::Found(owner) = found else {
            panic!("this process holds the client socket, got {found:?}");
        };
        assert_eq!(owner.uid, current_uid().unwrap());
        assert_eq!(owner.lineage[0].pid, std::process::id());
        assert_eq!(
            owner.lineage[0].group,
            process_group(std::process::id()).unwrap()
        );
        assert!(owner.lineage.len() > 1, "the test runner is its parent");
        assert_eq!(owner.lineage[1].pid, std::os::unix::process::parent_id());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn a_connection_nobody_holds_is_not_found() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let server = v4(listener.local_addr().unwrap());
        let client = TcpStream::connect(server).unwrap();
        let peer = v4(client.local_addr().unwrap());
        drop(client);
        let (accepted, _) = listener.accept().unwrap();
        drop(accepted);
        assert_eq!(
            loopback_socket_owner(peer, server),
            SocketOwnerLookup::NotFound,
            "a client that left before the lookup is not an owner"
        );
        let unrelated = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);
        assert_eq!(
            loopback_socket_owner(unrelated, server),
            SocketOwnerLookup::NotFound
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_socket_record_offsets_match_the_native_layout() {
        use std::mem::{offset_of, size_of};

        use libproc::net_info::SocketFDInfo;

        use super::platform::{
            FOREIGN_PORT_AT, FOREIGN_V4_AT, KIND_AT, LOCAL_PORT_AT, LOCAL_V4_AT, SOCKET_INFO_LEN,
            VERSION_FLAGS_AT,
        };

        let tcp = offset_of!(SocketFDInfo, psi.soi_proto.pri_tcp.tcpsi_ini);
        assert_eq!(size_of::<SocketFDInfo>(), SOCKET_INFO_LEN);
        assert_eq!(offset_of!(SocketFDInfo, psi.soi_kind), KIND_AT);
        assert_eq!(
            tcp + offset_of!(libproc::net_info::InSockInfo, insi_fport),
            FOREIGN_PORT_AT
        );
        assert_eq!(
            tcp + offset_of!(libproc::net_info::InSockInfo, insi_lport),
            LOCAL_PORT_AT
        );
        assert_eq!(
            tcp + offset_of!(libproc::net_info::InSockInfo, insi_vflag),
            VERSION_FLAGS_AT
        );
        let v4 = offset_of!(libproc::net_info::In4In6Addr, i46a_addr4);
        assert_eq!(
            tcp + offset_of!(libproc::net_info::InSockInfo, insi_faddr) + v4,
            FOREIGN_V4_AT
        );
        assert_eq!(
            tcp + offset_of!(libproc::net_info::InSockInfo, insi_laddr) + v4,
            LOCAL_V4_AT
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_proc_net_fields_are_the_kernel_spelling() {
        let (v4, mapped) = platform::fields_for_test(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 8080));
        let loopback = format!("{:08X}", u32::from_ne_bytes([127, 0, 0, 1]));
        assert_eq!(v4, format!("{loopback}:1F90"));
        assert!(mapped.ends_with(&format!("{loopback}:1F90")));
        assert_eq!(mapped.len(), 32 + 5);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    #[test]
    fn other_platforms_say_they_cannot_answer() {
        let any = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1);
        assert_eq!(
            loopback_socket_owner(any, any),
            SocketOwnerLookup::Unsupported
        );
    }
}
