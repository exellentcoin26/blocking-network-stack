//! Non-async Networking primitives for TCP/UDP communication.

#![no_std]

// MUST be the first module
mod fmt;

use core::{borrow::BorrowMut, cell::RefCell, fmt::Display, marker::PhantomData};

#[cfg(feature = "dhcpv4")]
use smoltcp::socket::dhcpv4::Socket as Dhcpv4Socket;
#[cfg(feature = "tcp")]
use smoltcp::socket::tcp::Socket as TcpSocket;
#[cfg(feature = "dns")]
use smoltcp::wire::DnsQueryType;
#[cfg(feature = "udp")]
use smoltcp::wire::IpEndpoint;
use smoltcp::{
    iface::{Interface, SocketHandle, SocketSet},
    time::Instant,
    wire::{IpAddress, IpCidr, Ipv4Address},
};

#[cfg(feature = "tcp")]
const LOCAL_PORT_MIN: u16 = 41000;
#[cfg(feature = "tcp")]
const LOCAL_PORT_MAX: u16 = 65535;

/// Non-async TCP/IP network stack
///
/// Mostly a convenience wrapper for `smoltcp`
pub struct Stack<'a, D: smoltcp::phy::Device> {
    device: RefCell<D>,
    network_interface: RefCell<Interface>,
    sockets: RefCell<SocketSet<'a>>,
    current_millis_fn: fn() -> u64,
    #[cfg(feature = "tcp")]
    local_port: RefCell<u16>,
    pub(crate) network_config: RefCell<ipv4::Configuration>,
    pub(crate) ip_info: RefCell<Option<ipv4::IpInfo>>,
    #[cfg(feature = "dhcpv4")]
    pub(crate) dhcp_socket_handle: RefCell<Option<SocketHandle>>,
    #[cfg(feature = "dhcpv4")]
    pub(crate) reset_dhcp: RefCell<bool>,
    #[cfg(feature = "dns")]
    dns_socket_handle: RefCell<Option<SocketHandle>>,
}

impl<'a, D: smoltcp::phy::Device> Stack<'a, D> {
    /// Creates new `WifiStack` instance.
    ///
    /// Handles optional DHCP/DNS features and sets up the
    /// configuration for the network interface.
    pub fn new(
        network_interface: Interface,
        device: D,
        #[allow(unused_mut)] mut sockets: SocketSet<'a>,
        current_millis_fn: fn() -> u64,
        random: u32,
    ) -> Stack<'a, D> {
        #[cfg(feature = "dhcpv4")]
        let mut dhcp_socket_handle: Option<SocketHandle> = None;
        #[cfg(feature = "dns")]
        let mut dns_socket_handle: Option<SocketHandle> = None;

        #[cfg(any(feature = "dhcpv4", feature = "dns"))]
        for (handle, socket) in sockets.iter_mut() {
            match socket {
                #[cfg(feature = "dhcpv4")]
                smoltcp::socket::Socket::Dhcpv4(_) => dhcp_socket_handle = Some(handle),
                #[cfg(feature = "dns")]
                smoltcp::socket::Socket::Dns(_) => dns_socket_handle = Some(handle),
                _ => {}
            }
        }

        let this = Self {
            device: RefCell::new(device),
            network_interface: RefCell::new(network_interface),
            network_config: RefCell::new(ipv4::Configuration::Client(
                ipv4::ClientConfiguration::DHCP(ipv4::DHCPClientSettings {
                    // FIXME: smoltcp currently doesn't have a way of giving a hostname through DHCP
                    hostname: Some(unwrap!("Espressif".try_into().ok())),
                }),
            )),
            ip_info: RefCell::new(None),
            #[cfg(feature = "dhcpv4")]
            dhcp_socket_handle: RefCell::new(dhcp_socket_handle),
            #[cfg(feature = "dhcpv4")]
            reset_dhcp: RefCell::new(false),
            sockets: RefCell::new(sockets),
            current_millis_fn,
            #[cfg(feature = "tcp")]
            local_port: RefCell::new(
                (random % (LOCAL_PORT_MAX - LOCAL_PORT_MIN) as u32) as u16 + LOCAL_PORT_MIN,
            ),
            #[cfg(feature = "dns")]
            dns_socket_handle: RefCell::new(dns_socket_handle),
        };

        this.reset();

        this
    }

    /// Update the interface configuration
    pub fn update_iface_configuration(
        &self,
        conf: &ipv4::Configuration,
    ) -> Result<(), WifiStackError> {
        let hw_address = self.network_interface.borrow_mut().hardware_addr();
        self.network_interface
            .borrow_mut()
            .set_hardware_addr(hw_address);
        info!("Set hardware address: {:?}", hw_address);

        self.reset(); // reset IP address

        #[cfg(feature = "dhcpv4")]
        {
            let mut dhcp_socket_handle_ref = self.dhcp_socket_handle.borrow_mut();
            let mut sockets_ref = self.sockets.borrow_mut();

            if let Some(dhcp_handle) = *dhcp_socket_handle_ref {
                // remove the DHCP client if we use a static IP
                if matches!(
                    conf,
                    ipv4::Configuration::Client(ipv4::ClientConfiguration::Fixed(_))
                ) {
                    sockets_ref.remove(dhcp_handle);
                    *dhcp_socket_handle_ref = None;
                }
            }

            // re-add the DHCP client if we use DHCP and it has been removed before
            if matches!(
                conf,
                ipv4::Configuration::Client(ipv4::ClientConfiguration::DHCP(_))
            ) && dhcp_socket_handle_ref.is_none()
            {
                let dhcp_socket = Dhcpv4Socket::new();
                let dhcp_socket_handle = sockets_ref.add(dhcp_socket);
                *dhcp_socket_handle_ref = Some(dhcp_socket_handle);
            }

            if let Some(dhcp_handle) = *dhcp_socket_handle_ref {
                let dhcp_socket = sockets_ref.get_mut::<Dhcpv4Socket>(dhcp_handle);
                info!("Reset DHCP client");
                dhcp_socket.reset();
            }
        }

        *self.network_config.borrow_mut() = conf.clone();
        Ok(())
    }

    /// Reset DHCP
    #[cfg(feature = "dhcpv4")]
    pub fn reset_dhcp(&self) {
        *self.reset_dhcp.borrow_mut() = true;
    }

    /// Reset the stack
    pub fn reset(&self) {
        debug!("Reset TCP stack");

        #[cfg(feature = "dhcpv4")]
        {
            let dhcp_socket_handle_ref = self.dhcp_socket_handle.borrow_mut();
            if let Some(dhcp_handle) = *dhcp_socket_handle_ref {
                self.with_mut(|_, _, sockets| {
                    let dhcp_socket = sockets.get_mut::<Dhcpv4Socket>(dhcp_handle);
                    debug!("Reset DHCP client");
                    dhcp_socket.reset();
                });
            }
        }

        self.with_mut(|interface, _, _| {
            interface.routes_mut().remove_default_ipv4_route();
            interface.update_ip_addrs(|addrs| {
                addrs.clear();
            });

            #[cfg(feature = "ipv6")]
            {
                unwrap!(interface.routes_mut().add_default_ipv6_route(
                    smoltcp::wire::Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 0,)
                ));

                let mut mac = [0u8; 6];
                match interface.hardware_addr() {
                    smoltcp::wire::HardwareAddress::Ethernet(hw_address) => {
                        mac.copy_from_slice(hw_address.as_bytes());
                    }
                }

                let a4 = ((mac[0] ^ 2) as u16) << 8 | mac[1] as u16;
                let a5 = (mac[2] as u16) << 8 | 0xff;
                let a6 = 0xfe << 8 | mac[3] as u16;
                let a7 = (mac[4] as u16) << 8 | mac[5] as u16;

                info!(
                    "IPv6 link-local address fe80::{:x}:{:x}:{:x}:{:x}",
                    a4, a5, a6, a7
                );

                interface.update_ip_addrs(|addrs| {
                    unwrap!(addrs.push(IpCidr::new(
                        smoltcp::wire::IpAddress::v6(0xfe80, 0, 0, 0, a4, a5, a6, a7),
                        64,
                    )));
                });
            }
        });
    }

    /// Retrieve all current IP addresses
    pub fn get_ip_addresses(&self, f: impl FnOnce(&[smoltcp::wire::IpCidr])) {
        self.with_mut(|interface, _, _| f(interface.ip_addrs()))
    }

    /// Convenience function to poll the DHCP socket.
    #[cfg(feature = "dhcpv4")]
    pub fn poll_dhcp(
        &self,
        interface: &mut Interface,
        sockets: &mut SocketSet<'a>,
    ) -> Result<(), WifiStackError> {
        let dhcp_socket_handle_ref = self.dhcp_socket_handle.borrow_mut();
        if let Some(dhcp_handle) = *dhcp_socket_handle_ref {
            let dhcp_socket = sockets.get_mut::<Dhcpv4Socket>(dhcp_handle);

            if *self.reset_dhcp.borrow() {
                *self.reset_dhcp.borrow_mut() = false;
                dhcp_socket.reset();
            }

            let event = dhcp_socket.poll();
            if let Some(event) = event {
                match event {
                    smoltcp::socket::dhcpv4::Event::Deconfigured => {
                        *self.ip_info.borrow_mut() = None;
                        interface.routes_mut().remove_default_ipv4_route();
                    }
                    smoltcp::socket::dhcpv4::Event::Configured(config) => {
                        let dns = config.dns_servers.first();
                        *self.ip_info.borrow_mut() = Some(ipv4::IpInfo {
                            ip: config.address.address().0.into(),
                            subnet: ipv4::Subnet {
                                gateway: unwrap!(config.router).0.into(),
                                mask: ipv4::Mask(config.address.prefix_len()),
                            },
                            dns: dns.map(|x| x.0.into()),
                            secondary_dns: config.dns_servers.get(1).map(|x| x.0.into()),
                        });

                        let address = config.address;
                        interface.borrow_mut().update_ip_addrs(|addrs| {
                            unwrap!(addrs.push(smoltcp::wire::IpCidr::Ipv4(address)));
                        });
                        if let Some(route) = config.router {
                            unwrap!(interface.routes_mut().add_default_ipv4_route(route));
                        }

                        #[cfg(feature = "dns")]
                        if let (Some(&dns), Some(dns_handle)) =
                            (dns, *self.dns_socket_handle.borrow())
                        {
                            sockets
                                .get_mut::<smoltcp::socket::dns::Socket>(dns_handle)
                                .update_servers(&[dns.into()]);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Create a new [Socket]
    #[cfg(feature = "tcp")]
    pub fn get_socket<'s, 'b>(
        &'s self,
        rx_buffer: &'b mut [u8],
        tx_buffer: &'b mut [u8],
    ) -> Socket<'s, 'a, 'b, D> {
        let socket = TcpSocket::new(
            smoltcp::socket::tcp::SocketBuffer::new(rx_buffer),
            smoltcp::socket::tcp::SocketBuffer::new(tx_buffer),
        );

        // SAFETY: The lifetime of the internal socket buffer is managed using the `PhantomData`
        // buffer lifetime.
        let socket = unsafe { core::mem::transmute::<TcpSocket<'b>, TcpSocket<'a>>(socket) };
        let socket_handle =
            self.with_mut(|_interface, _device, sockets| sockets.borrow_mut().add(socket));
        Socket {
            socket_handle,
            network: self,
            _buffer: PhantomData,
        }
    }

    /// Create a new [UdpSocket]
    #[cfg(feature = "udp")]
    pub fn get_udp_socket<'s, 'b>(
        &'s self,
        rx_meta: &'b mut [smoltcp::socket::udp::PacketMetadata],
        rx_buffer: &'b mut [u8],
        tx_meta: &'b mut [smoltcp::socket::udp::PacketMetadata],
        tx_buffer: &'b mut [u8],
    ) -> UdpSocket<'s, 'a, 'b, D>
    where
        'a: 's,
    {
        let socket = smoltcp::socket::udp::Socket::new(
            smoltcp::socket::udp::PacketBuffer::new(rx_meta, rx_buffer),
            smoltcp::socket::udp::PacketBuffer::new(tx_meta, tx_buffer),
        );

        let socket_handle = {
            use smoltcp::socket::udp::Socket as UdpSocket;

            // SAFETY: The lifetime of the internal socket buffer is managed using the `PhantomData`
            // buffer lifetime.
            let socket = unsafe { core::mem::transmute::<UdpSocket<'b>, UdpSocket<'a>>(socket) };

            self.with_mut(|_interface, _device, sockets| sockets.borrow_mut().add(socket))
        };

        UdpSocket {
            socket_handle,
            network: self,
            _buffer: PhantomData,
        }
    }

    /// Check if DNS is configured
    #[cfg(feature = "dns")]
    pub fn is_dns_configured(&self) -> bool {
        self.dns_socket_handle.borrow().is_some()
    }

    /// Configure DNS
    #[cfg(feature = "dns")]
    pub fn configure_dns(
        &'a self,
        servers: &[IpAddress],
        query_storage: &'a mut [Option<smoltcp::socket::dns::DnsQuery>],
    ) {
        if let Some(old_handle) = self.dns_socket_handle.take() {
            self.with_mut(|_interface, _device, sockets| sockets.remove(old_handle));
            // the returned socket get dropped and frees a slot for the new one
        }

        let dns = smoltcp::socket::dns::Socket::new(servers, query_storage);
        let handle = self.with_mut(|_interface, _device, sockets| sockets.add(dns));
        self.dns_socket_handle.replace(Some(handle));
    }

    /// Update the DNS servers
    #[cfg(feature = "dns")]
    pub fn update_dns_servers(&self, servers: &[IpAddress]) {
        if let Some(dns_handle) = *self.dns_socket_handle.borrow_mut() {
            self.with_mut(|_interface, _device, sockets| {
                sockets
                    .get_mut::<smoltcp::socket::dns::Socket>(dns_handle)
                    .update_servers(servers);
            });
        }
    }

    /// Perform a DNS query
    #[cfg(feature = "dns")]
    pub fn dns_query(
        &self,
        name: &str,
        query_type: DnsQueryType,
    ) -> Result<heapless::Vec<IpAddress, { smoltcp::config::DNS_MAX_RESULT_COUNT }>, WifiStackError>
    {
        use smoltcp::socket::dns;

        match query_type {
            // check if name is already an IP
            DnsQueryType::A => {
                if let Ok(ip) = name.parse::<Ipv4Address>() {
                    return Ok([ip.into()].into_iter().collect());
                }
            }
            #[cfg(feature = "ipv6")]
            DnsQueryType::Aaaa => {
                if let Ok(ip) = name.parse::<smoltcp::wire::Ipv6Address>() {
                    return Ok([ip.into()].into_iter().collect());
                }
            }
            _ => {}
        }

        let Some(dns_handle) = *self.dns_socket_handle.borrow() else {
            return Err(WifiStackError::DnsNotConfigured);
        };

        let query = self.with_mut(|interface, _device, sockets| {
            sockets
                .get_mut::<dns::Socket>(dns_handle)
                .start_query(interface.context(), name, query_type)
                .map_err(WifiStackError::DnsQueryError)
        })?;

        loop {
            self.work();

            let result = self.with_mut(|_interface, _device, sockets| {
                sockets
                    .get_mut::<dns::Socket>(dns_handle)
                    .get_query_result(query)
            });

            match result {
                Ok(addrs) => return Ok(addrs),               // query finished
                Err(dns::GetQueryResultError::Pending) => {} // query not finished
                Err(_) => return Err(WifiStackError::DnsQueryFailed),
            }
        }
    }

    /// Let the stack make progress
    ///
    /// Make sure to regularly call this function.
    pub fn work(&self) {
        loop {
            let did_work = self.with_mut(|interface, device, sockets| {
                let network_config = self.network_config.borrow().clone();
                if let ipv4::Configuration::Client(ipv4::ClientConfiguration::DHCP(_)) =
                    network_config
                {
                    #[cfg(feature = "dhcpv4")]
                    self.poll_dhcp(interface, sockets).ok();
                } else if let ipv4::Configuration::Client(ipv4::ClientConfiguration::Fixed(
                    settings,
                )) = network_config
                {
                    let addr = Ipv4Address::from_bytes(&settings.ip.octets());
                    if !interface.has_ip_addr(addr) {
                        let gateway = Ipv4Address::from_bytes(&settings.subnet.gateway.octets());
                        interface.routes_mut().add_default_ipv4_route(gateway).ok();
                        interface.update_ip_addrs(|addrs| {
                            unwrap!(addrs.push(IpCidr::new(addr.into(), settings.subnet.mask.0)));
                        });
                    }
                }
                interface.poll(
                    Instant::from_millis((self.current_millis_fn)() as i64),
                    device,
                    sockets,
                )
            });

            if !did_work {
                break;
            }
        }
    }

    #[cfg(feature = "tcp")]
    fn next_local_port(&self) -> u16 {
        self.local_port.replace_with(|local_port| {
            if *local_port == LOCAL_PORT_MAX {
                LOCAL_PORT_MIN
            } else {
                *local_port + 1
            }
        });
        *self.local_port.borrow()
    }

    #[allow(unused)]
    fn with<R>(&self, f: impl FnOnce(&Interface, &D, &SocketSet<'a>) -> R) -> R {
        f(
            &self.network_interface.borrow(),
            &self.device.borrow(),
            &self.sockets.borrow(),
        )
    }

    fn with_mut<R>(&self, f: impl FnOnce(&mut Interface, &mut D, &mut SocketSet<'a>) -> R) -> R {
        f(
            &mut self.network_interface.borrow_mut(),
            &mut self.device.borrow_mut(),
            &mut self.sockets.borrow_mut(),
        )
    }
}

/// Errors returned by functions in this module
#[derive(Debug, Copy, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum WifiStackError {
    /// An unknown error occurred, with the associated error code.
    Unknown(i32),

    /// An error occurred during Wi-Fi stack initialization.
    InitializationError,

    /// A common Wi-Fi error occured.
    DeviceError,

    /// Couldn't get the device's IP.
    MissingIp,

    /// DNS is not configured.
    #[cfg(feature = "dns")]
    DnsNotConfigured,

    /// An error occurred when starting a DNS query.
    #[cfg(feature = "dns")]
    DnsQueryError(smoltcp::socket::dns::StartQueryError),

    /// Cannot get result from a DNS query.
    #[cfg(feature = "dns")]
    DnsQueryFailed,
}

impl Display for WifiStackError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl<D: smoltcp::phy::Device> Stack<'_, D> {
    /// Retrieves the current interface configuration.
    pub fn get_iface_configuration(&self) -> Result<ipv4::Configuration, WifiStackError> {
        Ok(self.network_config.borrow().clone())
    }

    /// Sets a new interface configuration using the provided IPv4
    /// configuration.
    pub fn set_iface_configuration(
        &mut self,
        conf: &ipv4::Configuration,
    ) -> Result<(), WifiStackError> {
        self.update_iface_configuration(conf)
    }

    /// Checks if the interface is up (has IP information).
    pub fn is_iface_up(&self) -> bool {
        self.ip_info.borrow().is_some()
    }

    /// Retrieves the current IP information (IP address, subnet info).
    pub fn get_ip_info(&self) -> Result<ipv4::IpInfo, WifiStackError> {
        self.ip_info.borrow().ok_or(WifiStackError::MissingIp)
    }
}

/// A TCP socket
#[cfg(feature = "tcp")]
pub struct Socket<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> {
    socket_handle: SocketHandle,
    network: &'s Stack<'a, D>,
    _buffer: PhantomData<&'b ()>,
}

#[cfg(feature = "tcp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> Socket<'s, 'a, 'b, D> {
    /// Connect the socket
    pub fn open<'i>(&'i mut self, addr: IpAddress, port: u16) -> Result<(), IoError>
    where
        's: 'i,
    {
        {
            let res = self.network.with_mut(|interface, _device, sockets| {
                let sock = sockets.get_mut::<TcpSocket>(self.socket_handle);
                let cx = interface.context();
                let remote_endpoint = (addr, port);
                sock.set_ack_delay(Some(smoltcp::time::Duration::from_millis(100)));
                sock.connect(cx, remote_endpoint, self.network.next_local_port())
            });

            res.map_err(IoError::ConnectError)?;
        }

        loop {
            let can_send = self.network.with_mut(|_interface, _device, sockets| {
                let sock = sockets.get_mut::<TcpSocket>(self.socket_handle);
                sock.can_send()
            });

            if can_send {
                break;
            }

            self.work();
        }

        Ok(())
    }

    /// Listen on the given port. This blocks until there is a peer connected
    pub fn listen<'i>(&'i mut self, port: u16) -> Result<(), IoError>
    where
        's: 'i,
    {
        {
            let res = self.network.with_mut(|_interface, _device, sockets| {
                let sock = sockets.get_mut::<TcpSocket>(self.socket_handle);
                sock.listen(port)
            });

            res.map_err(IoError::ListenError)?;
        }

        loop {
            let can_send = self.network.with_mut(|_interface, _device, sockets| {
                let sock = sockets.get_mut::<TcpSocket>(self.socket_handle);
                sock.can_send()
            });

            if can_send {
                break;
            }

            self.work();
        }

        Ok(())
    }

    /// Listen on the given port. This doesn't block
    pub fn listen_unblocking<'i>(&'i mut self, port: u16) -> Result<(), IoError>
    where
        's: 'i,
    {
        {
            let res = self.network.with_mut(|_interface, _device, sockets| {
                let sock = sockets.get_mut::<TcpSocket>(self.socket_handle);
                sock.listen(port)
            });

            res.map_err(IoError::ListenError)?;
        }

        self.work();
        Ok(())
    }

    /// Closes the socket
    pub fn close(&mut self) {
        self.network.with_mut(|_interface, _device, sockets| {
            sockets.get_mut::<TcpSocket>(self.socket_handle).close();
        });

        self.work();
    }

    /// Disconnect the socket
    pub fn disconnect(&mut self) {
        self.network.with_mut(|_interface, _device, sockets| {
            sockets.get_mut::<TcpSocket>(self.socket_handle).abort();
        });

        self.work();
    }

    /// Checks if the socket is currently open
    pub fn is_open(&mut self) -> bool {
        self.network.with_mut(|_interface, _device, sockets| {
            sockets.get_mut::<TcpSocket>(self.socket_handle).is_open()
        })
    }

    /// Checks if the socket is currently connected
    pub fn is_connected(&mut self) -> bool {
        self.network.with_mut(|_interface, _device, sockets| {
            let socket = sockets.get_mut::<TcpSocket>(self.socket_handle);

            socket.may_recv() && socket.may_send()
        })
    }

    /// Delegates to [WifiStack::work]
    pub fn work(&mut self) {
        self.network.work()
    }
}

#[cfg(feature = "tcp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> Drop for Socket<'s, 'a, 'b, D> {
    fn drop(&mut self) {
        self.network
            .with_mut(|_interface, _device, sockets| sockets.remove(self.socket_handle));
    }
}

/// IO Errors
#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum IoError {
    SocketClosed,
    #[cfg(feature = "igmp")]
    MultiCastError(smoltcp::iface::MulticastError),
    #[cfg(feature = "tcp")]
    TcpRecvError,
    #[cfg(feature = "udp")]
    UdpRecvError(smoltcp::socket::udp::RecvError),
    #[cfg(feature = "tcp")]
    TcpSendError(smoltcp::socket::tcp::SendError),
    #[cfg(feature = "udp")]
    UdpSendError(smoltcp::socket::udp::SendError),
    #[cfg(feature = "tcp")]
    ConnectError(smoltcp::socket::tcp::ConnectError),
    #[cfg(feature = "udp")]
    BindError(smoltcp::socket::udp::BindError),
    #[cfg(feature = "tcp")]
    ListenError(smoltcp::socket::tcp::ListenError),
}

impl embedded_io::Error for IoError {
    fn kind(&self) -> embedded_io::ErrorKind {
        embedded_io::ErrorKind::Other
    }
}

#[cfg(feature = "tcp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> embedded_io::ErrorType for Socket<'s, 'a, 'b, D> {
    type Error = IoError;
}

#[cfg(feature = "tcp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> embedded_io::Read for Socket<'s, 'a, 'b, D> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        let current_millis = self.network.current_millis_fn;

        self.network.with_mut(|interface, device, sockets| {
            use smoltcp::socket::tcp::RecvError;
            loop {
                let now = smoltcp::time::Instant::from_millis((current_millis)() as i64);
                interface.poll(now, device, sockets);
                let socket = sockets.get_mut::<TcpSocket>(self.socket_handle);

                match socket.recv_slice(buf) {
                    Ok(0) => continue, // no data
                    Ok(n) => return Ok(n),
                    Err(RecvError::Finished) => return Err(IoError::SocketClosed), // eof
                    Err(RecvError::InvalidState) => return Err(IoError::TcpRecvError),
                }
            }
        })
    }
}

#[cfg(feature = "tcp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> embedded_io::ReadReady for Socket<'s, 'a, 'b, D> {
    fn read_ready(&mut self) -> Result<bool, Self::Error> {
        let current_millis = self.network.current_millis_fn;

        self.network.with_mut(|interface, device, sockets| {
            use smoltcp::socket::tcp::RecvError;
            let now = smoltcp::time::Instant::from_millis((current_millis)() as i64);
            interface.poll(now, device, sockets);
            let socket = sockets.get_mut::<TcpSocket>(self.socket_handle);

            match socket.peek(1) {
                Ok(s) => Ok(!s.is_empty()),
                Err(RecvError::Finished) => Err(IoError::SocketClosed),
                Err(RecvError::InvalidState) => Err(IoError::TcpRecvError),
            }
        })
    }
}

#[cfg(feature = "tcp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> embedded_io::Write for Socket<'s, 'a, 'b, D> {
    fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        loop {
            let (may_send, is_open, can_send) =
                self.network.with_mut(|interface, device, sockets| {
                    interface.poll(
                        Instant::from_millis((self.network.current_millis_fn)() as i64),
                        device,
                        sockets,
                    );

                    let socket = sockets.get_mut::<TcpSocket>(self.socket_handle);

                    (socket.may_send(), socket.is_open(), socket.can_send())
                });

            if may_send {
                break;
            }

            if !is_open || !can_send {
                return Err(IoError::SocketClosed);
            }
        }

        let mut written = 0;
        loop {
            self.flush()?;

            self.network.with_mut(|_interface, _device, sockets| {
                sockets
                    .get_mut::<TcpSocket>(self.socket_handle)
                    .send_slice(&buf[written..])
                    .map(|len| written += len)
                    .map_err(IoError::TcpSendError)
            })?;

            if written >= buf.len() {
                break;
            }
        }

        Ok(written)
    }

    fn flush(&mut self) -> Result<(), Self::Error> {
        loop {
            let res = self.network.with_mut(|interface, device, sockets| {
                interface.poll(
                    Instant::from_millis((self.network.current_millis_fn)() as i64),
                    device,
                    sockets,
                )
            });

            if !res {
                break;
            }
        }

        Ok(())
    }
}

#[cfg(feature = "tcp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> embedded_io::WriteReady
    for Socket<'s, 'a, 'b, D>
{
    fn write_ready(&mut self) -> Result<bool, Self::Error> {
        let (may_send, is_open, can_send) = self.network.with_mut(|interface, device, sockets| {
            interface.poll(
                Instant::from_millis((self.network.current_millis_fn)() as i64),
                device,
                sockets,
            );

            let socket = sockets.get_mut::<TcpSocket>(self.socket_handle);

            (socket.may_send(), socket.is_open(), socket.can_send())
        });

        if !is_open || !can_send {
            return Err(IoError::SocketClosed);
        }

        Ok(may_send)
    }
}

/// A UDP socket
#[cfg(feature = "udp")]
pub struct UdpSocket<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> {
    socket_handle: SocketHandle,
    network: &'s Stack<'a, D>,
    _buffer: PhantomData<&'b ()>,
}

#[cfg(feature = "udp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> UdpSocket<'s, 'a, 'b, D> {
    /// Binds the socket to the given port
    pub fn bind<'i>(&'i mut self, port: u16) -> Result<(), IoError>
    where
        's: 'i,
    {
        self.work();

        {
            let res = self.network.with_mut(|_interface, _device, sockets| {
                let sock = sockets.get_mut::<smoltcp::socket::udp::Socket>(self.socket_handle);
                sock.bind(port)
            });

            if let Err(err) = res {
                return Err(IoError::BindError(err));
            }
        }

        loop {
            let can_send = self.network.with_mut(|_interface, _device, sockets| {
                let sock = sockets.get_mut::<smoltcp::socket::udp::Socket>(self.socket_handle);
                sock.can_send()
            });

            if can_send {
                break;
            }

            self.work();
        }

        Ok(())
    }

    /// Close the socket
    pub fn close(&mut self) {
        self.network.with_mut(|_interface, _device, sockets| {
            sockets
                .get_mut::<smoltcp::socket::udp::Socket>(self.socket_handle)
                .close();
        });

        self.work();
    }

    /// Sends data on the socket to the given address
    pub fn send(&mut self, addr: IpAddress, port: u16, data: &[u8]) -> Result<(), IoError> {
        loop {
            self.work();

            let (can_send, packet_capacity, payload_capacity) =
                self.network.with_mut(|_interface, _device, sockets| {
                    let sock = sockets.get_mut::<smoltcp::socket::udp::Socket>(self.socket_handle);
                    (
                        sock.can_send(),
                        sock.packet_send_capacity(),
                        sock.payload_send_capacity(),
                    )
                });

            if can_send && packet_capacity > 0 && payload_capacity > data.len() {
                break;
            }
        }

        self.network
            .with_mut(|_interface, _device, sockets| {
                let endpoint = (addr, port);
                let endpoint: IpEndpoint = endpoint.into();

                sockets
                    .get_mut::<smoltcp::socket::udp::Socket>(self.socket_handle)
                    .send_slice(data, endpoint)
            })
            .map_err(IoError::UdpSendError)?;

        self.work();

        Ok(())
    }

    /// Receives a single datagram message on the socket
    pub fn receive(&mut self, data: &mut [u8]) -> Result<(usize, IpAddress, u16), IoError> {
        self.work();

        let res = self.network.with_mut(|_interface, _device, sockets| {
            sockets
                .get_mut::<smoltcp::socket::udp::Socket>(self.socket_handle)
                .recv_slice(data)
        });

        match res {
            Ok((len, endpoint)) => {
                let addr = endpoint.endpoint.addr;
                Ok((len, addr, endpoint.endpoint.port))
            }
            Err(e) => Err(IoError::UdpRecvError(e)),
        }
    }

    /// This function specifies a new multicast group for this socket to join
    #[cfg(feature = "igmp")]
    pub fn join_multicast_group(&mut self, addr: IpAddress) -> Result<bool, IoError> {
        self.work();

        let res = self.network.with_mut(|interface, device, _| {
            interface.join_multicast_group(
                device,
                addr,
                Instant::from_millis((self.network.current_millis_fn)() as i64),
            )
        });

        self.work();

        res.map_err(IoError::MultiCastError)
    }

    /// Delegates to [WifiStack::work]
    pub fn work(&mut self) {
        self.network.work()
    }
}

#[cfg(feature = "udp")]
impl<'s, 'a: 's, 'b: 's, D: smoltcp::phy::Device> Drop for UdpSocket<'s, 'a, 'b, D> {
    fn drop(&mut self) {
        self.network
            .with_mut(|_, _, sockets| sockets.borrow_mut().remove(self.socket_handle));
    }
}

/// IPv4 network configurations.
pub mod ipv4 {
    pub use core::net::Ipv4Addr;
    use core::{fmt::Display, str::FromStr};

    #[cfg(feature = "serde")]
    use serde::{Deserialize, Serialize};

    /// Represents a subnet mask.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub struct Mask(pub u8);

    impl FromStr for Mask {
        type Err = &'static str;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            s.parse::<u8>()
                .map_err(|_| "Invalid subnet mask")
                .map_or_else(Err, |mask| {
                    if (1..=32).contains(&mask) {
                        Ok(Mask(mask))
                    } else {
                        Err("Mask should be a number between 1 and 32")
                    }
                })
        }
    }

    impl Display for Mask {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "{}", self.0)
        }
    }

    impl TryFrom<Ipv4Addr> for Mask {
        type Error = ();

        fn try_from(ip: Ipv4Addr) -> Result<Self, Self::Error> {
            let octets = ip.octets();
            let addr: u32 = ((octets[0] as u32 & 0xff) << 24)
                | ((octets[1] as u32 & 0xff) << 16)
                | ((octets[2] as u32 & 0xff) << 8)
                | (octets[3] as u32 & 0xff);

            if addr.leading_ones() + addr.trailing_zeros() == 32 {
                Ok(Mask(addr.leading_ones() as u8))
            } else {
                Err(())
            }
        }
    }

    impl From<Mask> for Ipv4Addr {
        fn from(mask: Mask) -> Self {
            let addr: u32 = ((1 << (32 - mask.0)) - 1) ^ 0xffffffffu32;

            let (a, b, c, d) = (
                ((addr >> 24) & 0xff) as u8,
                ((addr >> 16) & 0xff) as u8,
                ((addr >> 8) & 0xff) as u8,
                (addr & 0xff) as u8,
            );

            Ipv4Addr::new(a, b, c, d)
        }
    }

    /// Represents a subnet consisting of a gateway and a mask.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub struct Subnet {
        /// The gateway IP address of the subnet.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub gateway: Ipv4Addr,
        /// The subnet mask associated with the subnet.
        pub mask: Mask,
    }

    impl Display for Subnet {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "{}/{}", self.gateway, self.mask)
        }
    }

    impl FromStr for Subnet {
        type Err = &'static str;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            let mut split = s.split('/');
            if let Some(gateway_str) = split.next() {
                if let Some(mask_str) = split.next() {
                    if split.next().is_none() {
                        if let Ok(gateway) = gateway_str.parse::<Ipv4Addr>() {
                            return mask_str.parse::<Mask>().map(|mask| Self { gateway, mask });
                        } else {
                            return Err("Invalid IP address format, expected XXX.XXX.XXX.XXX");
                        }
                    }
                }
            }

            Err("Expected <gateway-ip-address>/<mask>")
        }
    }

    /// Settings for a client in an IPv4 network.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub struct ClientSettings {
        /// The client's IPv4 address.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub ip: Ipv4Addr,

        /// The subnet associated with the client's IP address.
        pub subnet: Subnet,

        /// The primary DNS server for name resolution.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub dns: Option<Ipv4Addr>,

        /// The secondary DNS server for name resolution.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub secondary_dns: Option<Ipv4Addr>,
    }

    impl Default for ClientSettings {
        fn default() -> ClientSettings {
            ClientSettings {
                ip: Ipv4Addr::new(192, 168, 71, 200),
                subnet: Subnet {
                    gateway: Ipv4Addr::new(192, 168, 71, 1),
                    mask: Mask(24),
                },
                dns: Some(Ipv4Addr::new(8, 8, 8, 8)),
                secondary_dns: Some(Ipv4Addr::new(8, 8, 4, 4)),
            }
        }
    }

    /// Settings for the DHCP client.
    ///
    /// This struct contains the configuration for a DHCP client, including a
    /// hostname that can be sent during DHCP negotiations.
    #[derive(Default, Clone, Debug, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub struct DHCPClientSettings {
        pub hostname: Option<heapless::String<30>>,
    }

    /// Configuration for the client in an IPv4 network.
    ///
    /// This enum defines how the client's IP settings are obtained: either
    /// through DHCP, or as a fixed (static) configuration.
    #[derive(Clone, Debug, PartialEq, Eq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub enum ClientConfiguration {
        /// Use DHCP to obtain network settings.
        DHCP(DHCPClientSettings),

        /// Use a fixed configuration for network settings.
        Fixed(ClientSettings),
    }

    impl ClientConfiguration {
        /// Returns a reference to the fixed settings if the client is using a
        /// static configuration, `None` otherwise.
        pub fn as_fixed_settings_ref(&self) -> Option<&ClientSettings> {
            match self {
                Self::Fixed(client_settings) => Some(client_settings),
                _ => None,
            }
        }

        /// Returns a mutable reference to the fixed settings, creating a
        /// default fixed configuration if necessary.
        pub fn as_fixed_settings_mut(&mut self) -> &mut ClientSettings {
            match self {
                Self::Fixed(client_settings) => client_settings,
                _ => {
                    *self = ClientConfiguration::Fixed(Default::default());
                    self.as_fixed_settings_mut()
                }
            }
        }
    }

    impl Default for ClientConfiguration {
        fn default() -> ClientConfiguration {
            ClientConfiguration::DHCP(Default::default())
        }
    }

    /// Router configuration in an IPv4 network.
    #[derive(Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub struct RouterConfiguration {
        /// The subnet the router is responsible for.
        pub subnet: Subnet,

        /// Indicates whether DHCP is enabled on the router.
        pub dhcp_enabled: bool,

        /// The primary DNS server for the router.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub dns: Option<Ipv4Addr>,

        /// The secondary DNS server for the router.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub secondary_dns: Option<Ipv4Addr>,
    }

    impl Default for RouterConfiguration {
        fn default() -> RouterConfiguration {
            RouterConfiguration {
                subnet: Subnet {
                    gateway: Ipv4Addr::new(192, 168, 71, 1),
                    mask: Mask(24),
                },
                dhcp_enabled: true,
                dns: Some(Ipv4Addr::new(8, 8, 8, 8)),
                secondary_dns: Some(Ipv4Addr::new(8, 8, 4, 4)),
            }
        }
    }

    /// Represents the network configuration for a device.
    ///
    /// Holds either a client configuration (for devices connecting to a
    /// network) or a router configuration (for devices providing a network
    /// to other clients).
    #[derive(Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub enum Configuration {
        /// Configuration for a device acting as a client in the network.
        Client(ClientConfiguration),

        /// Configuration for a device acting as a router in the network.
        Router(RouterConfiguration),
    }

    impl Default for Configuration {
        fn default() -> Self {
            Self::Client(Default::default())
        }
    }

    /// Represents IPv4 information for a device.
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    #[cfg_attr(feature = "serde", derive(Deserialize, Serialize))]
    pub struct IpInfo {
        /// The IPv4 address of the device.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub ip: Ipv4Addr,

        /// The subnet mask associated with the device's IP address.
        pub subnet: Subnet,

        /// The primary DNS server for the device.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub dns: Option<Ipv4Addr>,

        /// The secondary DNS server for the device.
        #[cfg_attr(feature = "defmt", defmt(Debug2Format))]
        pub secondary_dns: Option<Ipv4Addr>,
    }
}
