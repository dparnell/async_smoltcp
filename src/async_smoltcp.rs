use core::task::{Context, Poll, Waker};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use futures::executor::block_on;
use futures::future::Future;
use futures::future::{BoxFuture, FutureExt};
use futures::lock::Mutex;

use smoltcp::iface::{Interface, InterfaceBuilder, Routes};
use smoltcp::time::Instant;
pub use smoltcp::wire::IpAddress;
pub use smoltcp::{Error as SmoltcpError, Result as SmoltcpResult};

use super::socket::{
    AsyncSocket, OnTcpSocketData, OnUdpSocketData, Socket, SocketConnectionError,
    SocketReceiveError, SocketSendError, SocketType,
};
use super::virtual_tun::{
    OnVirtualTunRead, OnVirtualTunWrite, VirtualTunInterface as VirtualTunDevice,
    VirtualTunReadError, VirtualTunWriteError,
};
#[cfg(feature = "vpn")]
use super::vpn_client::{
    PhyReceive, PhyReceiveError, PhySend, PhySendError, VpnClient, VpnConnectionError,
    VpnDisconnectionError,
};
#[cfg(feature = "log")]
use log::{debug, error, info, warn};
use smoltcp::phy::Device;
use smoltcp::phy::TapInterface as TapDevice;
use smoltcp::phy::TunInterface as TunDevice;
use smoltcp::socket::{
    SocketHandle, SocketSet, TcpSocket, TcpSocketBuffer, UdpSocket, UdpSocketBuffer,
};
pub use smoltcp::wire::{IpCidr, IpEndpoint, Ipv4Address, Ipv6Address};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::sync::Arc;
#[cfg(feature = "async")]
use tokio::io::{AsyncRead, AsyncWrite};

use crate::DEFAULT_MTU;

pub enum PollWaitError {
    NoPollWait,
}

pub type OnPollWait = Arc<dyn Fn(Duration) -> std::result::Result<(), PollWaitError> + Send + Sync>;

#[derive(Debug)]
pub enum SpinError {
    NoCallback,
    NoSocket,
    Unknown(String),
}

#[doc(hidden)]
pub fn __feed__<T, R>(arg: T, f: impl FnOnce(T) -> R) -> R {
    f(arg)
}

macro_rules! for_each_device {
    (
    $scrutinee:expr, $closure:expr $(,)?
) => {{
        use $crate::async_smoltcp::SmolStackWithDevice;
        use $crate::async_smoltcp::__feed__;

        match $scrutinee {
            SmolStackWithDevice::VirtualTun(inner) => __feed__(inner, $closure),
            SmolStackWithDevice::Tun(inner) => __feed__(inner, $closure),
            SmolStackWithDevice::Tap(inner) => __feed__(inner, $closure),
        }
    }};
}

struct OnSocketData {
    pub on_tcp: Option<OnTcpSocketData>,
    pub on_udp: Option<OnUdpSocketData>,
}

impl OnSocketData {
    pub fn tcp(t: Option<OnTcpSocketData>) -> OnSocketData {
        OnSocketData {
            on_tcp: t,
            on_udp: None,
        }
    }

    pub fn udp(t: Option<OnUdpSocketData>) -> OnSocketData {
        OnSocketData {
            on_tcp: None,
            on_udp: t,
        }
    }

    pub fn none() -> OnSocketData {
        OnSocketData {
            on_tcp: None,
            on_udp: None,
        }
    }
}

pub struct SmolStack<DeviceT>
where
    DeviceT: for<'d> Device<'d>,
{
    pub sockets: SocketSet<'static, 'static, 'static>,
    pub interface: Interface<'static, 'static, 'static, DeviceT>,
    socket_handles: HashMap<SocketHandle, (SocketType, OnSocketData)>,
    should_stack_thread_stop: Arc<AtomicBool>,
    read_wake_deque: Option<Arc<Mutex<VecDeque<Waker>>>>,
    write_wake_deque: Option<Arc<Mutex<VecDeque<Waker>>>>,
}

pub enum SmolStackWithDevice {
    VirtualTun(SmolStack<VirtualTunDevice>),
    Tun(SmolStack<TunDevice>),
    Tap(SmolStack<TapDevice>),
}

impl SmolStackWithDevice {
    pub fn run(stack: Arc<Mutex<Self>>) {
        std::thread::Builder::new()
            .name("stack_thread".to_string())
            .spawn(move || {
                let stack = stack.clone();
                let write_wake_deque_ = block_on(stack.clone().lock()).get_write_wake_deque();
                loop {
                    block_on(stack.clone().lock()).poll().unwrap();
                    block_on(stack.clone().lock()).spin_all();
                    //TODO: use wake deque future mutex or normal mutex?
                    let waker = block_on(write_wake_deque_.clone().lock()).pop_front();
                    match waker {
                        Some(waker) => {
                            //info!("waking from write wake deque");
                            waker.wake()
                        }
                        None => {}
                    }
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    if block_on(stack.clone().lock()).should_stop() {
                        info!("end of stack_thread");
                        break;
                    }
                }
            })
            .unwrap();
    }
}

impl SmolStackWithDevice {
    #[cfg(feature = "vpn")]
    pub fn new_from_vpn(
        interface_name: &str,
        address: Option<IpCidr>,
        default_v4_gateway: Option<Ipv4Address>,
        default_v6_gateway: Option<Ipv6Address>,
        mtu: Option<usize>,
        ip_addrs: Vec<IpCidr>,
        vpn_client: Arc<Mutex<dyn VpnClient + Send>>,
    ) -> SmolStackWithDevice {
        let mtu = mtu.unwrap_or(DEFAULT_MTU);
        let should_stack_thread_stop = Arc::new(AtomicBool::new(false));
        let vpn_client_ = vpn_client.clone();
        let on_virtual_tun_read = Arc::new(
            move |buffer: &mut [u8]| -> std::result::Result<usize, VirtualTunReadError> {
                let mut vpn_client_ = block_on(vpn_client_.lock());
                let mut size = 0;
                match vpn_client_.phy_receive(None, &mut |openvpn_buffer: &[u8]| {
                    for i in 0..openvpn_buffer.len() {
                        buffer[i] = openvpn_buffer[i]
                    }
                    size = openvpn_buffer.len();
                }) {
                    Ok(()) => Ok(size),
                    Err(PhyReceiveError::NoDataAvailable) => Err(VirtualTunReadError::WouldBlock),
                    //TODO: do not panic below and treat the error?
                    Err(PhyReceiveError::Unknown(error_string)) => {
                        panic!("openvpn_client.receive() unknown error: {}", error_string);
                    }
                }
            },
        );

        let vpn_client_ = vpn_client.clone();
        let on_virtual_tun_write = Arc::new(
            move |f: &mut dyn FnMut(&mut [u8]),
                  size: usize|
                  -> std::result::Result<(), VirtualTunWriteError> {
                let mut buffer = vec![0; size];
                f(buffer.as_mut_slice());
                match block_on(vpn_client_.lock()).phy_send(buffer.as_slice()) {
                    Ok(()) => Ok(()),
                    Err(PhySendError::Unknown(error_string)) => {
                        //TODO: not panic here, just treat the error
                        panic!(error_string);
                    }
                }
            },
        );

        let on_poll_wait = Arc::new(
            |duration: std::time::Duration| -> std::result::Result<(), PollWaitError> { Ok(()) },
        );

        let read_wake_deque = Arc::new(Mutex::new(VecDeque::<Waker>::new()));
        let write_wake_deque = Arc::new(Mutex::new(VecDeque::<Waker>::new()));

        //let data_from_socket_ = data_from_socket.clone();
        let read_wake_deque_ = read_wake_deque.clone();
        let read_wake_deque_ = read_wake_deque.clone();
        let on_dns_udp_data = Arc::new(
            move |buffer: &[u8], address: Option<IpEndpoint>| -> std::result::Result<(), ()> {
                info!("on_dns_udp_data for buffer with len {}", buffer.len());
                //data_from_socket_.lock().unwrap().push_back((buffer.iter().cloned().collect(), address));
                let waker = block_on(read_wake_deque_.lock()).pop_front();
                match waker {
                    Some(waker) => {
                        info!("waking from read wake deque");
                        waker.wake();
                    }
                    None => {}
                }
                Ok(())
            },
        );

        let device = VirtualTunDevice::new(
            interface_name,
            on_virtual_tun_read,
            on_virtual_tun_write,
            mtu,
        )
        .unwrap();

        let ip_address = address.unwrap_or(IpCidr::new(IpAddress::v4(192, 168, 69, 2), 24));
        let mut routes = Routes::new(BTreeMap::new());

        routes
            .add_default_ipv4_route(default_v4_gateway.unwrap())
            .unwrap();

        if default_v6_gateway.is_some() {
            routes
                .add_default_ipv6_route(default_v6_gateway.unwrap())
                .unwrap();
        }

        let interface = InterfaceBuilder::new(device)
            .ip_addrs(ip_addrs)
            .routes(routes)
            .finalize();

        let default_v4_gateway = default_v4_gateway.unwrap_or(Ipv4Address::new(192, 168, 69, 100));
        //TODO: find a good ipv6 to use
        let default_v6_gateway =
            default_v6_gateway.unwrap_or(Ipv6Address::new(1, 1, 1, 1, 1, 1, 1, 1));

        let socket_set = SocketSet::new(vec![]);

        SmolStackWithDevice::VirtualTun(SmolStack {
            sockets: socket_set,
            interface: interface,
            socket_handles: HashMap::new(),
            should_stack_thread_stop: Arc::new(AtomicBool::new(false)),
            read_wake_deque: Some(read_wake_deque.clone()),
            write_wake_deque: Some(write_wake_deque.clone()),
        })
    }

    #[cfg(feature = "tap")]
    pub fn new_tap(
        interface_name: &str,
        address: Option<IpCidr>,
        default_v4_gateway: Option<Ipv4Address>,
        default_v6_gateway: Option<Ipv6Address>,
        mtu: Option<usize>,
        ip_addrs: Vec<IpCidr>,
    ) -> SmolStackWithDevice {
        let mtu = mtu.unwrap_or(DEFAULT_MTU);

        let device = TapDevice::new(interface_name).unwrap();
        let ip_address = address.unwrap_or(IpCidr::new(IpAddress::v4(192, 168, 69, 2), 24));
        let mut routes = Routes::new(BTreeMap::new());

        routes
            .add_default_ipv4_route(default_v4_gateway.unwrap())
            .unwrap();

        if default_v6_gateway.is_some() {
            routes
                .add_default_ipv6_route(default_v6_gateway.unwrap())
                .unwrap();
        }

        let interface = InterfaceBuilder::new(device)
            .ip_addrs(ip_addrs)
            .routes(routes)
            .finalize();

        let default_v4_gateway = default_v4_gateway.unwrap_or(Ipv4Address::new(192, 168, 69, 100));
        let default_v6_gateway =
            default_v6_gateway.unwrap_or(Ipv6Address::new(1, 1, 1, 1, 1, 1, 1, 1));

        let socket_set = SocketSet::new(vec![]);

        SmolStackWithDevice::Tap(SmolStack {
            sockets: socket_set,
            interface: interface,
            socket_handles: HashMap::new(),
            should_stack_thread_stop: Arc::new(AtomicBool::new(false)),
            read_wake_deque: None,
            write_wake_deque: None,
        })
    }

    #[cfg(feature = "tun")]
    pub fn new_tun(
        interface_name: &str,
        address: Option<IpCidr>,
        default_v4_gateway: Option<Ipv4Address>,
        default_v6_gateway: Option<Ipv6Address>,
        mtu: Option<usize>,
        ip_addrs: Vec<IpCidr>,
    ) -> SmolStackWithDevice {
        let mtu = mtu.unwrap_or(DEFAULT_MTU);

        let device = TunDevice::new(interface_name).unwrap();

        let ip_address = address.unwrap_or(IpCidr::new(IpAddress::v4(192, 168, 69, 2), 24));
        let mut routes = Routes::new(BTreeMap::new());

        routes
            .add_default_ipv4_route(default_v4_gateway.unwrap())
            .unwrap();

        if default_v6_gateway.is_some() {
            routes
                .add_default_ipv6_route(default_v6_gateway.unwrap())
                .unwrap();
        }

        let interface = InterfaceBuilder::new(device)
            .ip_addrs(ip_addrs)
            .routes(routes)
            .finalize();

        let default_v4_gateway = default_v4_gateway.unwrap_or(Ipv4Address::new(192, 168, 69, 100));
        let default_v6_gateway =
            default_v6_gateway.unwrap_or(Ipv6Address::new(1, 1, 1, 1, 1, 1, 1, 1));

        let socket_set = SocketSet::new(vec![]);

        SmolStackWithDevice::Tun(SmolStack {
            sockets: socket_set,
            interface: interface,
            socket_handles: HashMap::new(),
            should_stack_thread_stop: Arc::new(AtomicBool::new(false)),
            read_wake_deque: None,
            write_wake_deque: None,
        })
    }

    fn get_write_wake_deque(&self) -> Arc<Mutex<VecDeque<Waker>>> {
        for_each_device!(self, |stack| stack
            .write_wake_deque
            .as_ref()
            .unwrap()
            .clone())
    }

    fn should_stop(&self) -> bool {
        for_each_device!(self, |stack| {
            stack.should_stack_thread_stop.load(Ordering::Relaxed)
        })
    }

    pub fn add_tcp_socket(
        &mut self,
        //stack: Arc<Mutex<Self>>,
        on_tcp_socket_data: Option<OnTcpSocketData>,
    ) -> Result<SocketHandle, ()> {
        for_each_device!(self, |stack_| {
            let rx_buffer = TcpSocketBuffer::new(vec![0; 65000]);
            let tx_buffer = TcpSocketBuffer::new(vec![0; 65000]);
            let socket = TcpSocket::new(rx_buffer, tx_buffer);
            let handle = stack_.sockets.add(socket);
            stack_.socket_handles.insert(
                handle,
                (SocketType::TCP, OnSocketData::tcp(on_tcp_socket_data)),
            );

            Ok(handle)
        })
    }

    pub fn add_udp_socket(
        &mut self,
        //stack: Arc<Mutex<Self>>,
        on_socket_data: Option<OnUdpSocketData>,
    ) -> Result<SocketHandle, ()> {
        for_each_device!(self, |stack_| {
            let rx_buffer = UdpSocketBuffer::new(Vec::new(), vec![0; 1024]);
            let tx_buffer = UdpSocketBuffer::new(Vec::new(), vec![0; 1024]);
            let socket = UdpSocket::new(rx_buffer, tx_buffer);
            let handle = stack_.sockets.add(socket);
            Ok(handle)
        })
    }

    pub fn tcp_socket_send(
        &mut self,
        socket_handle: SocketHandle,
        data: &[u8],
    ) -> Result<usize, SocketSendError> {
        for_each_device!(self, |stack| {
            let mut socket = stack.sockets.get::<TcpSocket>(socket_handle);
            socket.send_slice(data).map_err(|e| e.into())
        })
    }

    pub fn udp_socket_send(
        &mut self,
        socket_handle: SocketHandle,
        data: &[u8],
        addr: SocketAddr,
    ) -> Result<usize, SocketSendError> {
        for_each_device!(self, |stack| {
            let mut socket = stack.sockets.get::<UdpSocket>(socket_handle);
            socket
                .send_slice(data, addr.into())
                .map(|_| data.len())
                .map_err(|e| e.into())
        })
    }

    fn tcp_connect(
        &mut self,
        socket_handle: SocketHandle,
        addr: SocketAddr,
        src_port: u16,
    ) -> Result<(), SocketConnectionError> {
        for_each_device!(self, |stack| {
            let mut socket = stack.sockets.get::<TcpSocket>(socket_handle);
            socket.connect(addr, src_port).map_err(|e| e.into())
        })
    }

    pub fn poll(&mut self) -> SmoltcpResult<bool> {
        for_each_device!(self, |stack| {
            let timestamp = Instant::now();

            match stack.interface.poll(&mut stack.sockets, timestamp) {
                Ok(b) => Ok(b),
                Err(e) => {
                    panic!("{}", e);
                }
            }
        })
    }

    pub fn spin_tcp<'b, DeviceT: for<'d> Device<'d>>(
        stack: &mut SmolStack<DeviceT>,
        socket_handle: &SocketHandle,
        on_tcp_socket_data: OnTcpSocketData,
    ) -> std::result::Result<(), SpinError> {
        let mut socket = stack.sockets.get::<TcpSocket>(socket_handle.clone());
        if socket.can_recv() {
            socket
                .recv(|data| {
                    match on_tcp_socket_data(data) {
                        Ok(_) => {}
                        Err(_) => {}
                    }
                    (data.len(), ())
                })
                .unwrap();
        }
        Ok(())
    }

    pub fn spin_udp<DeviceT: for<'d> Device<'d>>(
        stack: &mut SmolStack<DeviceT>,
        socket_handle: &SocketHandle,
        on_udp_socket_data: OnUdpSocketData,
    ) -> std::result::Result<(), SpinError> {
        let mut socket = stack.sockets.get::<UdpSocket>(socket_handle.clone());
        if socket.can_recv() {
            let (buffer, endpoint) = socket.recv().unwrap();
            let addr = endpoint.addr;
            let port = endpoint.port;
            let addr: IpAddr = match addr {
                IpAddress::Ipv4(ipv4) => IpAddr::V4(ipv4.into()),
                IpAddress::Ipv6(ipv6) => IpAddr::V6(ipv6.into()),
                _ => return Err(SpinError::Unknown("spin address conversion error".into())),
            };
            match on_udp_socket_data(buffer, addr, port) {
                Ok(_) => {}
                Err(_) => return Err(SpinError::NoCallback),
            }
        }
        Ok(())
    }

    pub fn spin_all(&mut self) -> std::result::Result<(), SpinError> {
        for_each_device!(self, |stack| {
            let mut smol_socket_handles = Vec::<SocketHandle>::new();

            for (smol_socket_handle, _) in stack.socket_handles.iter() {
                smol_socket_handles.push(smol_socket_handle.clone());
            }

            for smol_socket_handle in smol_socket_handles.iter_mut() {
                let (socket_type, on_socket_data) = stack
                    .socket_handles
                    .get(&smol_socket_handle)
                    .ok_or(SpinError::NoSocket)
                    .unwrap();
                match socket_type {
                    SocketType::TCP => {
                        let on_tcp_socket_data = on_socket_data
                            .on_tcp
                            .as_ref()
                            .ok_or(SpinError::NoCallback)
                            .unwrap();
                        SmolStackWithDevice::spin_tcp(
                            stack,
                            smol_socket_handle,
                            on_tcp_socket_data.clone(),
                        );
                    }
                    SocketType::UDP => {
                        let on_udp_socket_data = on_socket_data
                            .on_udp
                            .as_ref()
                            .ok_or(SpinError::NoCallback)
                            .unwrap();
                        SmolStackWithDevice::spin_udp(
                            stack,
                            smol_socket_handle,
                            on_udp_socket_data.clone(),
                        );
                    }
                    _ => unimplemented!("socket type not implemented yet"),
                }
            }
        });
        Ok(())
    }
}

pub struct SmolSocket {
    socket_handle: SocketHandle,
    stack: Arc<Mutex<SmolStackWithDevice>>,
    queue: Arc<Mutex<(VecDeque<Vec<u8>>, VecDeque<Waker>)>>,
}

impl SmolSocket {
    pub fn new(
        stack: Arc<Mutex<SmolStackWithDevice>>,
        stack_ref: &mut SmolStackWithDevice,
    ) -> Result<SmolSocket, ()> {
        //TODO: DOES THIS MUTEX BLOCK A FUTURE????
        let queue = Arc::new(Mutex::new((
            VecDeque::<Vec<u8>>::new(),
            VecDeque::<Waker>::new(),
        )));
        let queue_ = queue.clone();
        let on_data = Arc::new(move |data: &[u8]| -> Result<usize, SocketReceiveError> {
            //TODO: can/should I block here?
            let mut queue = block_on(queue_.lock());
            queue.0.push_back(data.to_vec());
            if let Some(waker) = queue.1.pop_front() {
                waker.wake();
            }
            Ok(data.len())
        });
        let socket_handle = stack_ref.add_tcp_socket(Some(on_data));
        //smol_socket.map_err(|_|())
        Ok(SmolSocket {
            socket_handle: socket_handle.map_err(|_| ())?,
            stack: stack.clone(),
            queue: queue.clone(),
            //wake_deque: Arc::new(Mutex::new(VecDeque::new()))
        })
    }
    /*
    pub async fn on_lock<F, Fut>(&mut self, f: F)
    where
        F: Fn(&mut LockedSmolSocket)-> Fut,
        Fut: Future<Output = ()>,
    {
        let stack_ref = self.stack.lock().await;
        let mut locked_smol_socket = LockedSmolSocket {
            socket_handle: self.socket_handle.clone(),
            stack: &stack_ref,
        };
        f(&mut locked_smol_socket).await
    }
    */
}

#[cfg(feature = "async")]
impl AsyncRead for SmolSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        //self.stack.so
        let mut queue = match self.queue.lock().boxed().as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(queue) => queue,
        };
        if let Some(packet) = queue.0.pop_front() {
            //TODO: can we always put eberything here without error?
            buf.put_slice(packet.as_slice());
            Poll::Ready(Ok(()))
        } else {
            queue.1.push_back(cx.waker().clone());
            Poll::Pending
        }
    }
}
#[cfg(feature = "async")]
impl AsyncWrite for SmolSocket {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let mut stack = match self.stack.lock().boxed().as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(stack) => stack,
        };
        //TODO: map error
        stack.tcp_socket_send(self.socket_handle.clone(), buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

pub type SafeSmolStackWithDevice = SmolStackWithDevice;

//TODO: instead, implement Send for the C types?
unsafe impl Send for SafeSmolStackWithDevice {}

#[cfg(feature = "async")]
impl AsyncSocket for SmolSocket {
    fn tcp_socket_send<'d>(
        &'d mut self,
        data: &'d [u8],
    ) -> Pin<Box<dyn Future<Output = Result<usize, SocketSendError>> + Send + 'd>> {
        async move {
            self.stack
                .lock()
                .await
                .tcp_socket_send(self.socket_handle, data)
                .map_err(|e| e.into())
        }
        .boxed()
    }
    fn tcp_connect<'d>(
        &'d mut self,
        addr: SocketAddr,
        src_port: u16,
    ) -> Pin<Box<dyn Future<Output = Result<(), SocketConnectionError>> + Send + 'd>> {
        async move {
            self.stack
                .lock()
                .await
                .tcp_connect(self.socket_handle, addr, src_port)
                .map_err(|e| e.into())
        }
        .boxed()
    }
    fn tcp_receive<'d>(
        &'d mut self,
        f: &'d dyn Fn(&[u8]),
    ) -> Pin<Box<dyn Future<Output = Result<usize, SocketReceiveError>> + Send + 'd>> {
        unimplemented!("would block arc and then leave it blocked for too long");
    }

    fn udp_socket_send<'d>(
        &'d mut self,
        data: &'d [u8],
        addr: SocketAddr,
    ) -> Pin<Box<dyn Future<Output = Result<usize, SocketSendError>> + Send + 'd>> {
        async move {
            self.stack
                .lock()
                .await
                .udp_socket_send(self.socket_handle, data, addr)
                .map_err(|e| e.into())
        }
        .boxed()
    }

    fn udp_receive<'d>(
        &'d mut self,
        f: &'d dyn Fn(&[u8], SocketAddr, u16),
    ) -> Pin<Box<dyn Future<Output = Result<usize, SocketReceiveError>> + Send + 'd>> {
        unimplemented!("would block arc and then leave it blocked for too long");
    }
}

impl From<SmoltcpError> for SocketSendError {
    fn from(e: SmoltcpError) -> SocketSendError {
        match e {
            SmoltcpError::Exhausted => SocketSendError::Exhausted,
            _ => SocketSendError::Unknown(format!("{}", e)),
        }
    }
}

impl From<SmoltcpError> for SocketConnectionError {
    fn from(e: SmoltcpError) -> SocketConnectionError {
        match e {
            SmoltcpError::Exhausted => SocketConnectionError::Exhausted,
            _ => SocketConnectionError::Unknown(format!("{}", e)),
        }
    }
}
