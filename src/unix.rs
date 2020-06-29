use std::io;
use std::mem;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::{AsRawFd, RawFd};
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

use nix::sys::socket as sock;
use nix::sys::uio::IoVec;

/// On unixes we bind to the multicast address, which causes multicast packets to be filtered
fn bind_multicast(socket: &Socket, addr: &SocketAddr) -> io::Result<()> {
    socket.bind(&socket2::SockAddr::from(*addr))
}

pub struct MulticastOptions {
    pub read_timeout: Duration,
    pub loopback: bool,
    pub buffer_size: usize,
}

impl Default for MulticastOptions {
    fn default() -> Self {
        MulticastOptions {
            read_timeout: Duration::from_millis(100),
            loopback: false,
            buffer_size: 512,
        }
    }
}

fn create_on_interfaces(
    options: MulticastOptions,
    interfaces: Vec<Ipv4Addr>,
    multicast_address: SocketAddrV4,
) -> io::Result<MulticastSocket> {
    let socket = Socket::new(Domain::ipv4(), Type::dgram(), Some(Protocol::udp()))?;
    socket.set_read_timeout(Some(options.read_timeout))?;
    socket.set_multicast_loop_v4(options.loopback)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;

    sock::setsockopt(socket.as_raw_fd(), sock::sockopt::Ipv4PacketInfo, &true)
        .map_err(nix_to_io_error)?;

    for interface in &interfaces {
        socket.join_multicast_v4(multicast_address.ip(), &interface)?;
    }

    bind_multicast(&socket, &multicast_address.into())?;

    Ok(MulticastSocket {
        socket,
        interfaces,
        multicast_address,
        buffer_size: options.buffer_size,
    })
}

pub struct MulticastSocket {
    socket: socket2::Socket,
    interfaces: Vec<Ipv4Addr>,
    multicast_address: SocketAddrV4,
    buffer_size: usize,
}

#[derive(Debug)]
pub enum Interface {
    Default,
    Ip(Ipv4Addr),
    Index(u32),
}

#[derive(Debug)]
pub struct Message {
    pub data: Vec<u8>,
    pub origin_address: SocketAddrV4,
    pub interface: Interface,
}

pub fn all_ipv4_interfaces() -> io::Result<Vec<Ipv4Addr>> {
    let interfaces = get_if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|i| match i.ip() {
            std::net::IpAddr::V4(v4) => Some(v4),
            _ => None,
        })
        .collect();
    Ok(interfaces)
}

impl MulticastSocket {
    pub fn all_interfaces(multicast_address: SocketAddrV4) -> io::Result<Self> {
        let interfaces = all_ipv4_interfaces()?;
        create_on_interfaces(Default::default(), interfaces, multicast_address)
    }

    pub fn with_options(
        multicast_address: SocketAddrV4,
        interfaces: Vec<Ipv4Addr>,
        options: MulticastOptions,
    ) -> io::Result<Self> {
        create_on_interfaces(options, interfaces, multicast_address)
    }
}

fn nix_to_io_error(e: nix::Error) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e)
}

impl MulticastSocket {
    pub fn receive(&self) -> io::Result<Message> {
        let mut data_buffer = vec![0; self.buffer_size];
        let mut control_buffer = nix::cmsg_space!(libc::in_pktinfo);

        let message = sock::recvmsg(
            self.socket.as_raw_fd(),
            &[IoVec::from_mut_slice(&mut data_buffer)],
            Some(&mut control_buffer),
            sock::MsgFlags::empty(),
        )
        .map_err(nix_to_io_error)?;

        let origin_address = match message.address {
            Some(sock::SockAddr::Inet(v4)) => Some(v4.to_std()),
            _ => None,
        };
        let origin_address = match origin_address {
            Some(SocketAddr::V4(v4)) => v4,
            _ => SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
        };

        let mut interface = Interface::Default;

        for cmsg in message.cmsgs() {
            if let sock::ControlMessageOwned::Ipv4PacketInfo(pktinfo) = cmsg {
                interface = Interface::Index(pktinfo.ipi_ifindex as u32);
            }
        }

        Ok(Message {
            data: data_buffer[0..message.bytes].to_vec(),
            origin_address,
            interface,
        })
    }

    pub fn send(&self, buf: &[u8], interface: &Interface) -> io::Result<usize> {
        match interface {
            Interface::Default => self.socket.set_multicast_if_v4(&Ipv4Addr::UNSPECIFIED)?,
            Interface::Ip(address) => self.socket.set_multicast_if_v4(address)?,
            Interface::Index(index) => {
                sock::setsockopt(self.socket.as_raw_fd(), ProtoMulticastIfIndex, index)
                    .map_err(nix_to_io_error)?
            }
        };

        self.socket
            .send_to(buf, &SocketAddr::from(self.multicast_address).into())
    }

    pub fn broadcast(&self, buf: &[u8]) -> io::Result<()> {
        for interface in &self.interfaces {
            self.send(buf, &Interface::Ip(*interface))?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ProtoMulticastIfIndex;

impl sock::SetSockOpt for ProtoMulticastIfIndex {
    type Val = u32;

    fn set(&self, fd: RawFd, val: &Self::Val) -> nix::Result<()> {
        let mut req: libc::ip_mreqn = unsafe { mem::zeroed() };
        req.imr_ifindex = *val as i32;
        let result = unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_IF,
                &req as *const _ as *const _,
                mem::size_of_val(&req) as libc::socklen_t,
            )
        };
        nix::errno::Errno::result(result).map(drop)
    }
}