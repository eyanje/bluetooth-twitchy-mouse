use bluer::{Address, AddressType};
use bluer::l2cap::{Socket, SocketAddr, Stream, StreamListener};
use libc::EBADF;
use std::io;
use std::pin::{Pin};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

use hid_device_id::bluetooth::psm;

// Control and interrupt listeners

/// Returns a new socket listening on the HID control psm.
pub fn new_control_listener(address: Address) -> io::Result<StreamListener> {
    let control_listener_socket = Socket::new_stream()?;
    control_listener_socket.bind(SocketAddr {
        addr: address,
        addr_type: AddressType::BrEdr,
        psm: psm::HID_CONTROL,
        cid: 0,
    })?;
    control_listener_socket.listen(0)
}

/// Returns a new socket listening on the HID interrupt psm.
pub fn new_interrupt_listener(address: Address) -> io::Result<StreamListener> {
    let interrupt_listener_socket = Socket::new_stream()?;
    interrupt_listener_socket.bind(SocketAddr {
        addr: address,
        addr_type: AddressType::BrEdr,
        psm: psm::HID_INTERRUPT,
        cid: 0,
    })?;
    interrupt_listener_socket.listen(0)
}


/// Struct to perform async IO on a potentially uninitialized stream. Reads will block forever, and
/// writes will potentially error with EBADF.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IOOption<S>(Option<S>);

#[allow(dead_code)]
impl <S> IOOption<S> {
    pub const fn new(opt: Option<S>) -> Self {
        Self(opt)
    }

    pub const fn new_none() -> Self {
        Self(None)
    }

    pub const fn new_some(s: S) -> Self {
        Self(Some(s))
    }

    /// Consumes this IOOption and return the option inside.
    pub fn inner(self) -> Option<S> {
        self.0
    }
    
    /// Borrows the option inside as a reference.
    pub const fn inner_ref(&self) -> &Option<S> {
        &self.0
    }
    
    /// Borrows the option inside as a mutable reference.
    pub fn inner_mut(&mut self) -> &mut Option<S> {
        &mut self.0
    }

    pub const fn is_none(&self) -> bool {
        self.0.is_none()
    }

    pub const fn is_some(&self) -> bool {
        self.0.is_some()
    }

    pub fn unwrap(self) -> S {
        self.0.unwrap()
    }

    pub fn unwrap_ref(&self) -> &S {
        match self.0.as_ref() {
            None => panic!("Called `IOOption::unwrap_ref()` on a `None` value."),
            Some(s) => s,
        }
    }

    pub fn unwrap_mut(&mut self) -> &mut S {
        match self.0.as_mut() {
            None => panic!("Called `IOOption::unwrap_mut()` on a `None` value."),
            Some(s) => s,
        }
    }

    pub fn unwrap_pinned(self: Pin<&Self>) -> Pin<&S> {
        unsafe { self.map_unchecked(Self::unwrap_ref) }
    }

    pub fn unwrap_pinned_mut(self: Pin<&mut Self>) -> Pin<&mut S> {
        unsafe { self.map_unchecked_mut(Self::unwrap_mut) }
    }

    pub fn replace(&mut self, value: S) -> Self {
        Self(self.0.replace(value))
    }
}

impl <S: AsyncRead> AsyncRead for IOOption<S> {
    /// Read from this stream.
    ///
    /// If the option is Some, then the read proceeds as normal, potentially pending if the
    /// underlying stream is pending. If the option is None, then the read is pending.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.is_some() {
            self.unwrap_pinned_mut().poll_read(cx, buf)
        } else {
            Poll::Pending
        }
    }
}

// Return the EBADF error if the Option is None.
impl <S: AsyncWrite> AsyncWrite for IOOption<S> {
    
    /// Write data to this stream.
    ///
    /// While the option is None, this call remains pending.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.is_some() {
            self.unwrap_pinned_mut().poll_write(cx, buf)
        } else {
            Poll::Ready(Err(io::Error::from_raw_os_error(EBADF)))
        }
    }
    
    /// Flush this option stream.
    ///
    /// While the option is None, this call remains pending.
    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        if self.is_some() {
            self.unwrap_pinned_mut().poll_flush(cx)
        } else {
            Poll::Ready(Err(io::Error::from_raw_os_error(EBADF)))
        }
    }

    /// Shut down this option stream.
    ///
    /// While the option is None, this call remains pending.
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        if self.is_some() {
            self.unwrap_pinned_mut().poll_shutdown(cx)
        } else {
            Poll::Ready(Err(io::Error::from_raw_os_error(EBADF)))
        }
    }

}


pub enum Event {
    /// Single-time event for when an interrupt channel is opened.
    /// Not sure if only one interrupt connection ever exists, but the main loop only accepts one.
    InterruptEstablished(Stream),
}

#[derive(Debug)]
pub struct Connection {
    peer_address: Address,
    // Must order control before interrupt so that we drop interrupt first.
    pub control_socket: Stream,
    pub interrupt_socket: IOOption<Stream>,
    pub event_rx: mpsc::Receiver<Event>,
    event_tx: mpsc::Sender<Event>,
}

/// Struct to control connections, as a central server.
/// Doesn't contain the connection. Rather, use events to communicate with a connection handler.
pub struct ConnectionControl<T> {
    pub peer_address: Address,
    pub data: T,
    pub event_tx: mpsc::Sender<Event>,
}


impl Connection {
    pub fn new(peer_address: Address, control_socket: Stream) -> Self {
        let (event_tx, event_rx) = mpsc::channel(16);
        Self {
            peer_address: peer_address,
            control_socket,
            interrupt_socket: IOOption::new_none(),
            event_rx,
            event_tx,
        }
    }

    /// Returns the address of this Connection's peer.
    pub fn peer_address(&self) -> Address {
        self.peer_address
    }

    /// Returns a clone of the event socket, which can be written to.
    pub fn clone_event_sender(&self) -> mpsc::Sender<Event> {
        self.event_tx.clone()
    }


    /// Create a new ConnectionControl with the specified data.
    pub fn new_controller<T>(&self, data: T) -> ConnectionControl<T> {
        ConnectionControl {
            peer_address: self.peer_address,
            data,
            event_tx: self.clone_event_sender(),
        }
    }
}



/*

pub struct Listener {
    control_listener: Socket,
    interrupt_listener: Socket,
}

impl Listener {
    /// Construct a new Listener to listen for control and interrupt socket connections.
    /// Will likely fail if there exists another listener socket
    pub fn new() -> Self {
        let control_listener_socket = Socket::new_stream()?;
        control_listener_socket.bind(SocketAddr {
            addr: Address::any(),
            addr_type: AddressType::BrEdr,
            psm: psm::HID_CONTROL,
            cid: 0,
        })?;
        // set_security, set_power_forced_active, set_recv_mtu, set_flow_control, set_recv_buffer,
        // set_l2cap_opts, set_link_mode
        let control_listener = control_listener_socket.listen(0)?;
        println!("Listening for control connection");
    
        println!("Establishing interrupt connection");
        let interrupt_listener_socket = Socket::new_stream()?;
        interrupt_listener_socket.bind(SocketAddr {
            addr: Address::any(),
            addr_type: AddressType::BrEdr,
            psm: psm::HID_INTERRUPT,
            cid: 0,
        })?;
        let interrupt_listener = interrupt_listener_socket.listen(0)?;
        println!("Listening for interrupt connection");

        Self {
            control_listener,
            interrupt_listener,
        }
    }
}
    
/// Struct to facilitate access to a control socket.
#[derive(Debug)]
struct ControlSocket<'a> {
    internal: Stream,
}

impl AsyncRead for ControlSocket {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.internal.poll_read(cx, buf)
    }
}

/// Struct to facilitate access to an interrupt socket,
/// which might not yet exist.
#[derive(Debug, Default)]
struct InterruptSocket {
    internal: OptionStream,
}

impl InterruptSocket {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_stream(stream: Stream) -> Self {
        Self {
            internal: OptionStream::new_with(Stream)
        }
    }
}

impl AsyncRead for InterruptSocket {
    /// Read from this InterruptSocket.
    ///
    /// This call remains pending until the InterruptSocket is initialized.
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.internal.poll_read(cx, buf)
    }
}

impl AsyncWrite for InterruptSocket {
    /// Write to this InterruptSocket.
    ///
    /// This call remains pending until the InterruptSocket is initialized.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        self.internal.poll_write(cx, buf)
    }
    
    /// Flush this InterruptSocket.
    ///
    /// This call remains pending until the InterruptSocket is initialized.
    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Error>> {
        self.internal.poll_flush(cx, buf)
    }

    /// Shut down this InterruptSocket.
    ///
    /// This call remains pending until the InterruptSocket is initialized.
    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Error>> {
        self.internal.poll_shutdown(cx)
    }

}

*/
