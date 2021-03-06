#![deny(missing_docs)]

//! Use monome devices (Grid or Arc) in rust.

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, Instant};
use std::sync::Arc;

use futures::future::Either;
use tokio::net::UdpSocket;
use tokio::prelude::*;
use tokio::timer::Delay;

use futures::sync::mpsc::{Sender, Receiver};

use rosc::decoder::decode;
use rosc::encoder::encode;
use rosc::{OscMessage, OscPacket, OscType};

use crossbeam::queue::ArrayQueue;

use futures::*;
use log::*;

/// The default port at which serialosc is running.
pub const SERIALOSC_PORT: i32 = 12002;

/// Port from which this library will start searching for free port when needed.
const START_PORT: i32 = 10_000;

/// After this number of milliseconds without receiving a device info message from seriaolc, this
/// library considers all the devices to have been received.
const DEVICE_ENUMERATION_TIMEOUT_MS: u64 = 500;

/// From a x and y position, and a stride, returns the offset at which the element is in an array.
fn toidx(x: i32, y: i32, width: i32) -> usize {
    (y * width + x) as usize
}

/// Returns an osc packet from a address and arguments
fn build_osc_message(addr: &str, args: Vec<OscType>) -> OscPacket {
    let message = OscMessage {
        addr: addr.to_owned(),
        args: Some(args),
    };
    OscPacket::Message(message)
}

fn new_bound_socket() -> UdpSocket {
    let mut port = START_PORT;
    loop {
        let server_addr = format!("127.0.0.1:{}", port).parse().unwrap();
        let bind_result = UdpSocket::bind(&server_addr);
        match bind_result {
            Ok(socket) => break socket,
            Err(e) => {
                warn!("bind error: {}", e.to_string());
                if port > 65535 {
                    panic!("Could not bind socket: port exhausted");
                }
            }
        }
        port += 1;
    }
}

/// An enum filled when a device has been added or removed, along with its name.
#[derive(Debug)]
pub enum DeviceChangeEvent {
    /// A device has been added on the host and recognized by serialosc, and is now available for use.
    Added(String),
    /// A device has been removed on the host and is now unavailable for use.
    Removed(String),
}

#[derive(Debug)]
struct MonomeInfo {
    port: Option<i32>,
    host: Option<String>,
    prefix: Option<String>,
    id: Option<String>,
    size: Option<(i32, i32)>,
    rotation: Option<i32>,
}

impl MonomeInfo {
    fn new() -> MonomeInfo {
        MonomeInfo {
            port: None,
            host: None,
            prefix: None,
            id: None,
            size: None,
            rotation: None,
        }
    }
    fn complete(&self) -> bool {
        self.port.is_some()
            && self.host.is_some()
            && self.prefix.is_some()
            && self.id.is_some()
            && self.size.is_some()
            && self.rotation.is_some()
    }
    fn fill(&mut self, packet: OscPacket) {
        match packet {
            OscPacket::Message(message) => {
                if message.addr.starts_with("/sys") {
                    if let Some(args) = message.args {
                        if message.addr.starts_with("/sys/port") {
                            if let OscType::Int(port) = args[0] {
                                self.port = Some(port);
                            }
                        } else if message.addr.starts_with("/sys/host") {
                            if let OscType::String(ref host) = args[0] {
                                self.host = Some(host.to_string());
                            }
                        } else if message.addr.starts_with("/sys/id") {
                            if let OscType::String(ref id) = args[0] {
                                self.id = Some(id.to_string());
                            }
                        } else if message.addr.starts_with("/sys/prefix") {
                            if let OscType::String(ref prefix) = args[0] {
                                self.prefix = Some(prefix.to_string());
                            }
                        } else if message.addr.starts_with("/sys/rotation") {
                            if let OscType::Int(rotation) = args[0] {
                                self.rotation = Some(rotation);
                            }
                        } else if message.addr.starts_with("/sys/size") {
                            if let OscType::Int(x) = args[0] {
                                if let OscType::Int(y) = args[1] {
                                    self.size = Some((x, y));
                                }
                            }
                        }
                    }
                }
            }
            OscPacket::Bundle(_bundle) => {
                error!("Bundle during setup!?");
            }
        }
    }
}

/// `Transport` implements the network input and output to and from serialosc.
struct Transport {
    /// The port for this device. This is the first free port starting at 10000.
    device_port: i32,
    /// This is the socket with with we send and receive to and from the device.
    socket: UdpSocket,
    /// This is the channel we use to forward the received OSC messages to the client object.
    tx: Arc<ArrayQueue<Vec<u8>>>,
    /// This is where Transport receives the OSC messages to send.
    rx: Receiver<Vec<u8>>,
}

impl Transport {
    pub fn new(
        device_port: i32,
        socket: UdpSocket,
        tx: Arc<ArrayQueue<Vec<u8>>>,
        rx: Receiver<Vec<u8>>,
    ) -> Transport {
        Transport {
            device_port,
            socket,
            tx,
            rx,
        }
    }
}

impl Future for Transport {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        loop {
            match self.rx.poll() {
                Ok(fut) => {
                    match fut {
                        Async::Ready(b) => {
                            let device_address = format!("127.0.0.1:{}", self.device_port);
                            let addr: SocketAddr = device_address.parse().unwrap();
                            // This happens when shutting down usually
                            if b.is_some() {
                                let _amt =
                                    try_ready!(self.socket.poll_send_to(&b.unwrap(), &addr));
                            } else {
                                break;
                            }
                        }
                        Async::NotReady => {
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("Error on future::mpsc {:?}", e);
                }
            }
        }

        loop {
            let mut buf = vec![0; 1024];
            match self.socket.poll_recv(&mut buf) {
                Ok(fut) => match fut {
                    Async::Ready(_ready) => match self.tx.push(buf) {
                        Ok(()) => {
                            continue;
                        }
                        Err(e) => {
                            error!("receive from monome, {}", e);
                        }
                    },
                    Async::NotReady => {
                        return Ok(Async::NotReady);
                    }
                },
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }
}

/// The client object for a Monome grid device
pub struct Monome {
    /// The name of this device
    name: String,
    /// The type of this device
    device_type: MonomeDeviceType,
    /// The port at which this device is running at
    port: i32,
    /// The host for this device (usually localhost)
    host: String,
    /// The ID of this device
    id: String,
    /// The prefix set for this device
    prefix: String,
    /// The current rotation for this device. This can be 0, 90, 180 or 270.
    rotation: i32,
    /// THe x and y size for this device.
    size: (i32, i32),
    /// A channel that allows receiving serialized OSC messages from a device.
    q: Arc<ArrayQueue<Vec<u8>>>,
    /// A channel that allows sending serialized OSC messages to a device.
    tx: Sender<Vec<u8>>,
}

/// Whether a key press is going up or down
#[derive(Debug)]
pub enum KeyDirection {
    /// The key has been released.
    Up,
    /// The key has been pressed.
    Down,
}

/// An event received from a monome device. This can be either a key press or release, a tilt
/// event, an encoder rotation event, or an encoder press or release.
pub enum MonomeEvent {
    /// A key press or release
    GridKey {
        /// The horizontal offset at which the key has been pressed.
        x: i32,
        /// The vertical offset at which the key has been pressed.
        y: i32,
        /// Whether the key has been pressed (`Down`), or released (`Up`).
        direction: KeyDirection,
    },
    /// A update about the tilt of this device.
    Tilt {
        /// Which sensor sent this tilt update.
        n: i32,
        /// The pitch of this device.
        x: i32,
        /// The roll of this device.
        y: i32,
        /// The yaw of this device.
        z: i32,
    },
    /// An encoder delta information
    EncoderDelta {
        /// Which encoder is sending the event.
        n: usize,
        /// The delta of this movement on this encoder.
        delta: i32,
    },
    /// A key press on an encoder (only available on some older devices).
    EncoderKey {
        /// Which encoder is sending the event.
        n: usize,
        /// Whether the encoder key has been pressed (`Down`), or released (`Up`).
        direction: KeyDirection,
    },
}

/// Converts an to a Monome method argument to a OSC address fragment and suitable OscType,
/// performing an eventual conversion.
pub trait IntoAddrAndArgs<'a, B> {
    /// Converts an to a Monome method argument to a OSC address fragment and suitable OscType,
    /// performing an eventual conversion.
    fn as_addr_frag_and_args(&self) -> (String, B);
}

/// Used to make a call with an intensity value, adds the `"level/"` portion in the address.
impl<'a> IntoAddrAndArgs<'a, OscType> for i32 {
    fn as_addr_frag_and_args(&self) -> (String, OscType) {
        ("level/".to_string(), OscType::Int(*self))
    }
}

/// Used to make an on/off call, converts to 0 or 1.
impl<'a> IntoAddrAndArgs<'a, OscType> for bool {
    fn as_addr_frag_and_args(&self) -> (String, OscType) {
        ("".to_string(), OscType::Int(if *self { 1 } else { 0 }))
    }
}

impl<'a> IntoAddrAndArgs<'a, Vec<OscType>> for &'a [u8; 64] {
    fn as_addr_frag_and_args(&self) -> (String, Vec<OscType>) {
        // TODO: error handling both valid: either 64 or more intensity values, or 8 masks
        let mut osctype_vec = Vec::with_capacity(64);
        for item in self.iter().map(|i| OscType::Int(i32::from(*i))) {
            osctype_vec.push(item);
        }
        ("level/".to_string(), osctype_vec)
    }
}

impl<'a> IntoAddrAndArgs<'a, Vec<OscType>> for u8 {
    fn as_addr_frag_and_args(&self) -> (String, Vec<OscType>) {
        // TODO: error handling both valid: either 64 or more intensity values, or 8 masks
        let mut osctype_vec = Vec::with_capacity(1);
        osctype_vec.push(OscType::Int(i32::from(*self)));
        ("".to_string(), osctype_vec)
    }
}

impl<'a> IntoAddrAndArgs<'a, Vec<OscType>> for &'a [u8; 8] {
    fn as_addr_frag_and_args(&self) -> (String, Vec<OscType>) {
        // TODO: error handling both valid: either 64 or more intensity values, or 8 masks
        let mut osctype_vec = Vec::with_capacity(8);
        for item in self.iter().map(|i| OscType::Int(i32::from(*i))) {
            osctype_vec.push(item);
        }
        ("".to_string(), osctype_vec)
    }
}

/// Used to convert vectors of bools for on/off calls, packs into a 8-bit integer mask.
impl<'a> IntoAddrAndArgs<'a, Vec<OscType>> for &'a [bool; 64] {
    fn as_addr_frag_and_args(&self) -> (String, Vec<OscType>) {
        // TODO: error handling
        assert!(self.len() >= 64);
        let mut masks = [0 as u8; 8];
        for i in 0..8 {
            // for each row
            let mut mask: u8 = 0;
            for j in (0..8).rev() {
                // create mask
                let idx = toidx(j, i, 8);
                mask = mask.rotate_left(1) | if self[idx] { 1 } else { 0 };
            }
            masks[i as usize] = mask;
        }
        let mut osctype_vec = Vec::with_capacity(8);
        for item in masks.iter().map(|i| OscType::Int(i32::from(*i))) {
            osctype_vec.push(item);
        }
        ("".to_string(), osctype_vec)
    }
}

/// A type of device, either Grid (of various size), Arc (with 2 or 4 encoders), or unknown.
#[derive(PartialEq, Clone)]
pub enum MonomeDeviceType {
    /// The type for a monome grid.
    Grid,
    /// The type for a monome arc.
    Arc,
    /// Unknown device, please file an issue.
    Unknown,
}

impl From<&str> for MonomeDeviceType {
    fn from(string: &str) -> MonomeDeviceType {
        if string.contains("arc") {
            MonomeDeviceType::Arc
        } else {
            MonomeDeviceType::Grid
        }
    }
}

impl fmt::Display for MonomeDeviceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            if *self == MonomeDeviceType::Grid {
                "grid"
            } else {
                "arc"
            }
        )
    }
}

impl fmt::Debug for MonomeDeviceType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

#[derive(Debug)]
/// A struct with basic informations about a Monome device, available without having set it up
pub struct MonomeDevice {
    /// Name of the device with serial number
    name: String,
    /// Device type
    device_type: MonomeDeviceType,
    /// Port at which this device is available
    port: i32,
}

impl fmt::Display for MonomeDevice {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {} ({})", self.name, self.device_type, self.port)
    }
}

impl MonomeDevice {
    fn new(name: &str, device_type: &str, port: i32) -> MonomeDevice {
        MonomeDevice {
            name: name.to_string(),
            device_type: device_type.into(),
            port,
        }
    }
    /// Return the device type.
    pub fn device_type(&self) -> MonomeDeviceType {
        return self.device_type.clone();
    }
    /// Return the device name.
    pub fn name(&self) -> String {
        return self.name.clone();
    }
    /// Return the port on which this device is.
    pub fn port(&self) -> i32 {
        return self.port;
    }
}

impl Monome {
    /// Register for device added/removed notifications, on a non-standard serialosc port
    ///
    /// # Arguments
    ///
    /// * `serialosc_port`: the port on which serialosc is running
    /// - `callback`: a function that is called whenever a device is added or removed.
    ///
    /// # Example
    ///
    /// Print a message, on a machine where serialosc runs on port 1234.
    ///
    /// ```no_run
    /// use monome::Monome;
    /// use monome::DeviceChangeEvent;
    /// Monome::register_device_change_callback_with_port(1234, |event| {
    ///     match event {
    ///         DeviceChangeEvent::Added(id) => {
    ///             println!("Device {} added", id);
    ///         }
    ///         DeviceChangeEvent::Removed(id) => {
    ///             println!("Device {} removed", id);
    ///         }
    ///     }
    /// });
    /// ```
    pub fn register_device_change_callback_with_port(
        serialosc_port: i32,
        callback: fn(DeviceChangeEvent),
    ) {
        let mut socket = new_bound_socket();

        thread::spawn(move || {
            let server_port = socket.local_addr().unwrap().port();
            let addr = format!("127.0.0.1:{}", serialosc_port).parse().unwrap();
            let packet = build_osc_message(
                "/serialosc/notify",
                vec![
                    OscType::String("127.0.0.1".to_string()),
                    OscType::Int(i32::from(server_port)),
                ],
            );
            let mut bytes: Vec<u8>;
            // True if we've received a add or remove message from serialosc recently, and we need
            // to tell it to notify this program in the future.
            // This is necessary, because other messages can be received on this socket, notably the
            // undocumented /sys/connect and /sys/disconnect messages (without any arguments).
            let mut need_notify_msg = true;
            loop {
                bytes = encode(&packet).unwrap();
                if need_notify_msg {
                    socket = socket
                        .send_dgram(bytes, &addr)
                        .wait()
                        .map(|(s, _)| s)
                        .unwrap();
                    need_notify_msg = false;
                }

                socket = socket.recv_dgram(vec![0u8; 1024]).and_then(|(socket, data, _, _)| {
                    match decode(&data).unwrap() {
                        OscPacket::Message(message) => {
                            if let Some(ref args) = message.args {
                                if message.addr.starts_with("/serialosc/add") {
                                    need_notify_msg = true;
                                    if let OscType::String(ref id) = args[0] {
                                        callback(DeviceChangeEvent::Added(id.to_string()));
                                    }
                                } else if message.addr.starts_with("/serialosc/remove") {
                                    if let OscType::String(ref id) = args[0] {
                                        need_notify_msg = true;
                                        callback(DeviceChangeEvent::Removed(id.to_string()));
                                    }
                                } else {
                                    debug!("⇦ Unexpected message receive on device change event socket {:?}", message);
                                }
                            }
                        }
                        _ => {
                            debug!("⇦ Could not decode {:?}", data);
                        }
                    }
                    Ok(socket)
                })
                .wait()
                    .map(|socket| socket)
                    .unwrap();
            }
        });
    }
    /// Register for device added/removed notifications, on the default serialosc port
    ///
    /// # Arguments
    ///
    /// - `callback`: a function that is called whenever a device is added or removed.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use monome::Monome;
    /// use monome::DeviceChangeEvent;
    /// Monome::register_device_change_callback(|event| {
    ///     match event {
    ///         DeviceChangeEvent::Added(id) => {
    ///             println!("Device {} added", id);
    ///         }
    ///         DeviceChangeEvent::Removed(id) => {
    ///             println!("Device {} removed", id);
    ///         }
    ///     }
    /// });
    /// ```
    pub fn register_device_change_callback(callback: fn(DeviceChangeEvent)) {
        Monome::register_device_change_callback_with_port(SERIALOSC_PORT, callback)
    }
    fn setup<S>(
        prefix: S,
        device: &MonomeDevice,
    ) -> Result<(MonomeInfo, UdpSocket, String, MonomeDeviceType, i32), String>
    where
        S: Into<String>,
    {
        let (name, device_type, port) = (
            device.name.to_string(),
            device.device_type.clone(),
            device.port,
        );

        let device_address = format!("127.0.0.1:{}", port);
        let add = device_address.parse();
        let addr: SocketAddr = add.unwrap();

        let socket = new_bound_socket();
        let server_port = socket.local_addr().unwrap().port();
        let packet = build_osc_message("/sys/port", vec![OscType::Int(i32::from(server_port))]);
        let bytes: Vec<u8> = encode(&packet).unwrap();
        let socket = socket
            .send_dgram(bytes, &addr)
            .wait()
            .map(|(s, _)| s)
            .unwrap();

        let local_addr = socket.local_addr().unwrap().ip();
        let packet = build_osc_message("/sys/host", vec![OscType::String(local_addr.to_string())]);
        let bytes: Vec<u8> = encode(&packet).unwrap();
        let socket = socket
            .send_dgram(bytes, &addr)
            .wait()
            .map(|(s, _)| s)
            .unwrap();

        let packet = build_osc_message("/sys/prefix", vec![OscType::String(prefix.into())]);
        let bytes: Vec<u8> = encode(&packet).unwrap();
        let socket = socket
            .send_dgram(bytes, &addr)
            .wait()
            .map(|(s, _)| s)
            .unwrap();

        let packet = build_osc_message("/sys/info", vec![]);
        let bytes: Vec<u8> = encode(&packet).unwrap();
        let mut socket = socket
            .send_dgram(bytes, &addr)
            .wait()
            .map(|(s, _)| s)
            .unwrap();

        let mut info = MonomeInfo::new();

        // Loop until we've received all the /sys/info messages
        let socket = loop {
            socket = socket
                .recv_dgram(vec![0u8; 1024])
                .and_then(|(socket, data, _, _)| {
                    let packet = decode(&data).unwrap();
                    info.fill(packet);
                    Ok(socket)
                })
                .wait()
                .map(|socket| socket)
                .unwrap();

            if info.complete() {
                break socket;
            }
        };

        Ok((info, socket, name, device_type, port))
    }
    /// Enumerate all monome devices on a non-standard serialosc port.
    ///
    /// If successful, this returns a list of MonomeDevice, which contain basic informations about
    /// the device: type, serial number, port allocated by serialosc.
    ///
    /// # Arguments
    ///
    /// * `serialosc_port`: the port on which serialosc is running
    ///
    /// # Example
    ///
    /// Enumerate and display all monome device on port 1234:
    ///
    /// ```no_run
    ///     use monome::Monome;
    ///     let enumeration = Monome::enumerate_devices_with_port(1234);
    ///     match enumeration {
    ///         Ok(devices) => {
    ///             for device in &devices {
    ///                println!("{}", device);
    ///             }
    ///         }
    ///         Err(e) => {
    ///             eprintln!("Error: {}", e);
    ///         }
    ///     }
    /// ```
    pub fn enumerate_devices_with_port(serialosc_port: i32) -> Result<Vec<MonomeDevice>, String> {
        let socket = new_bound_socket();
        let mut devices = Vec::<MonomeDevice>::new();
        let server_port = socket.local_addr().unwrap().port();
        let server_ip = socket.local_addr().unwrap().ip().to_string();

        let packet = build_osc_message(
            "/serialosc/list",
            vec![
                OscType::String(server_ip),
                OscType::Int(i32::from(server_port)),
            ],
        );

        let bytes: Vec<u8> = encode(&packet).unwrap();

        let addr = format!("127.0.0.1:{}", serialosc_port).parse().unwrap();
        let (mut socket, _) = socket.send_dgram(bytes, &addr).wait().unwrap();
        // loop until we find the device list message. It can be that some other messages are
        // received in the meantime, for example, tilt messages, or keypresses. Ignore them
        // here. If no message have been received for 500ms, consider we have all the messages and
        // carry on.
        loop {
            let fut = socket.recv_dgram(vec![0u8; 1024]).select2(Delay::new(
                Instant::now() + Duration::from_millis(DEVICE_ENUMERATION_TIMEOUT_MS),
            ));
            let task = tokio::runtime::current_thread::block_on_all(fut);
            socket = match task {
                Ok(Either::A(((s, data, _, _), _))) => {
                    socket = s;
                    let packet = decode(&data).unwrap();

                    match packet {
                        OscPacket::Message(message) => {
                            if message.addr == "/serialosc/device" {
                                if let Some(args) = &message.args {
                                    if let [OscType::String(ref name), OscType::String(ref device_type), OscType::Int(port)] =
                                        args.as_slice()
                                    {
                                        devices.push(MonomeDevice::new(name, device_type, *port));
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                        OscPacket::Bundle(_bundle) => {
                            eprintln!("Unexpected bundle received during setup");
                        }
                    };

                    socket
                }
                Ok(Either::B(_)) => {
                    // timeout
                    break;
                }
                Err(e) => {
                    panic!("{:?}", e);
                }
            };
        }

        Ok(devices)
    }
    /// Enumerate all monome devices on the standard port on which serialosc runs (12002).
    ///
    /// If successful, this returns a list of MonomeDevice, which contain basic informations about
    /// the device: type, serial number, port allocated by serialosc.
    ///
    /// # Arguments
    ///
    /// * `serialosc_port`: the port on which serialosc is running
    ///
    /// # Example
    ///
    /// Enumerate and display all monome device on port 1234:
    ///
    /// ```no_run
    ///     use monome::Monome;
    ///     let enumeration = Monome::enumerate_devices();
    ///     match enumeration {
    ///         Ok(devices) => {
    ///             for device in &devices {
    ///                println!("{}", device);
    ///             }
    ///         }
    ///         Err(e) => {
    ///             eprintln!("Error: {}", e);
    ///         }
    ///      }
    /// ```
    pub fn enumerate_devices() -> Result<Vec<MonomeDevice>, String> {
        Monome::enumerate_devices_with_port(SERIALOSC_PORT)
    }
    /// Sets up the "first" monome device, with a particular prefix. When multiple devices are
    /// plugged in, it's unclear which one is activated, however this is rare.
    ///
    /// # Arguments
    ///
    /// * `prefix` - the prefix to use for this device and this application
    ///
    /// # Example
    ///
    /// Set up a monome, with a prefix of "/prefix":
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let m = Monome::new("/prefix");
    ///
    /// match m {
    ///   Ok(monome) => {
    ///     println!("{:?}", monome);
    ///   }
    ///   Err(s) => {
    ///     println!("Could not setup the monome: {}", s);
    ///   }
    /// }
    /// ```
    pub fn new<S>(prefix: S) -> Result<Monome, String>
    where
        S: Into<String>,
    {
        Monome::new_with_port(prefix, SERIALOSC_PORT)
    }
    /// Sets up the "first" monome device, with a particular prefix and a non-standard port for
    /// serialosc. When multiple devices are plugged in, it's unclear which one is activated,
    /// however this is rare.
    ///
    /// # Arguments
    ///
    /// * `prefix` - the prefix to use for this device and this application
    /// * `serialosc_port` - the port at which serialosc can be reached.
    ///
    /// # Example
    ///
    /// Set up a monome, with a prefix of "/prefix", and specify an explicit port on which
    /// serialosc can be reached (here, the default of 12002):
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let m = Monome::new_with_port("/prefix", 12002);
    ///
    /// match m {
    ///   Ok(monome) => {
    ///     println!("{:?}", monome);
    ///   }
    ///   Err(s) => {
    ///     println!("Could not setup the monome: {}", s);
    ///   }
    /// }
    /// ```
    pub fn new_with_port<S>(prefix: S, serialosc_port: i32) -> Result<Monome, String>
    where
        S: Into<String>,
    {
        let devices = Monome::enumerate_devices_with_port(serialosc_port)?;
        if devices.is_empty() {
            return Err("No devices detected".to_string());
        }
        Monome::from_device(&devices[0], prefix.into())
    }
    /// Get a monome instance on which to call commands, from a `MonomeDevice`.
    ///
    /// # Arguments
    ///
    /// * `device`: a `MonomeDevice` acquired through `enumerate_devices`.
    /// * `prefix`: the prefix to use for this device and this application
    ///
    /// # Example
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let enumeration = Monome::enumerate_devices();
    /// match enumeration {
    ///     Ok(devices) => {
    ///         for device in &devices {
    ///             println!("{}", device);
    ///             match Monome::from_device(device, "prefix") {
    ///                 Ok(m) => {
    ///                     println!("Monome setup:\n{}", m);
    ///                 }
    ///                 Err(e) => {
    ///                     println!("Error setting up {} ({})", device, e);
    ///                 }
    ///             }
    ///         }
    ///     }
    ///     Err(e) => {
    ///         eprintln!("Error: {}", e);
    ///     }
    /// }
    /// ```
    pub fn from_device<S>(device: &MonomeDevice, prefix: S) -> Result<Monome, String>
    where
        S: Into<String>,
    {
        let prefix = prefix.into();
        let (info, socket, name, device_type, device_port) = Monome::setup(&*prefix, device)?;

        let (sender, receiver) = futures::sync::mpsc::channel(16);
        let q = Arc::new(ArrayQueue::new(32));
        let q2 = q.clone();
        let t = Transport::new(device_port, socket, q, receiver);

        thread::spawn(move || {
            tokio::run(t.map_err(|e| error!("server error = {:?}", e)));
        });

        Ok(Monome {
            tx: sender,
            q: q2,
            name: name.to_string(),
            device_type,
            host: info.host.unwrap(),
            id: info.id.unwrap(),
            port: device_port,
            prefix,
            rotation: info.rotation.unwrap(),
            size: info.size.unwrap(),
        })
    }

    /// Set a single led on a grid on or off.
    ///
    /// # Arguments
    ///
    /// - `x` - the horizontal position of the led to set.
    /// - `y` - the vertical positino of the led to set.
    /// - `arg` - either a bool, true to set a led On, false to set it Off, or a number between 0
    /// and 16, 0 being led off, 16 being full led brightness.
    ///
    /// # Example
    ///
    /// Set the led on the second row and second column to On, and also the third row and second
    /// column to mid-brightness:
    ///
    /// ```no_run
    /// # use monome::Monome;
    /// # let mut monome = Monome::new("/prefix").unwrap();
    /// monome.set(1 /* 2nd, 0-indexed */,
    ///            1 /* 2nd, 0-indexed */,
    ///            true);
    /// monome.set(1 /* 2nd, 0-indexed */,
    ///            2 /* 3nd, 0-indexed */,
    ///            8);
    /// ```
    pub fn set<'a, A>(&mut self, x: i32, y: i32, arg: A)
    where
        A: IntoAddrAndArgs<'a, OscType>,
    {
        if self.device_type != MonomeDeviceType::Grid {
            error!("Called grid method on something that is not an grid.");
            return;
        }
        let (frag, arg) = arg.as_addr_frag_and_args();
        self.send(
            &format!("/grid/led/{}set", frag).to_string(),
            vec![OscType::Int(x), OscType::Int(y), arg],
        );
    }

    /// Set all led of the grid to an intensity
    ///
    /// # Arguments
    ///
    /// * `intensity` - either a bool, true for led On or false for led Off, or a number between 0
    /// and 16, 0 being led off, and 16 being full led brightness.
    ///
    /// # Example
    ///
    /// On a grid, set all led to medium brightness, then turn it on:
    ///
    /// ```no_run
    /// # use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// monome.all(8);
    /// monome.all(false);
    /// ```
    pub fn all<'a, A>(&mut self, arg: A)
    where
        A: IntoAddrAndArgs<'a, OscType>,
    {
        if self.device_type != MonomeDeviceType::Grid {
            error!("Called grid method on something that is not an grid.");
            return;
        }
        let (frag, arg) = arg.as_addr_frag_and_args();
        self.send(&format!("/grid/led/{}all", frag).to_string(), vec![arg]);
    }

    /// Set all the leds of a monome in one call.
    ///
    /// # Arguments
    ///
    /// * `leds` - a vector of 64 booleans for a monome 64, 128 elements for a monome 128, and 256
    /// elements for a monome 256, packed in row order.
    ///
    /// # Example
    ///
    /// One a monome 128, do a checkerboard pattern:
    ///
    /// ```no_run
    /// # use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// let mut grid = [false; 128];
    /// for i in 0..128 {
    ///   grid[i] = (i + 1) % 2 == 0;
    /// }
    /// monome.set_all(&grid);
    /// ```
    pub fn set_all(&mut self, leds: &[bool]) {
        if self.device_type != MonomeDeviceType::Grid {
            error!("Called grid method on something that is not an grid.");
            return;
        }
        let width_in_quad = self.size.0 / 8;
        let height_in_quad = self.size.1 / 8;
        let width = self.size.0;
        let quad_size: i32 = 8;

        let mut masks = [0 as u8; 8];
        for a in 0..height_in_quad {
            for b in 0..width_in_quad {
                for i in 0..8 {
                    // for each row
                    let mut mask: u8 = 0;
                    for j in (0..8).rev() {
                        // create mask
                        let idx = toidx(b * quad_size + j, a * quad_size + i, width);
                        mask = mask.rotate_left(1) | if leds[idx] { 1 } else { 0 };
                    }
                    masks[i as usize] = mask;
                }
                self.map(b * 8, a * 8, &masks);
            }
        }
    }

    /// Set all the leds of a monome in one call.
    ///
    /// # Arguments
    ///
    /// * `leds` - a vector of 64 integers in [0, 15] for a monome 64, 128 elements for a monome
    /// 128, and 256 elements for a monome 256, packed in row order.
    ///
    /// # Example
    ///
    /// One a monome 128, do a gradient
    ///
    /// ```no_run
    /// # use monome::Monome;
    ///
    /// let mut m = Monome::new("/prefix").unwrap();
    /// let mut grid: Vec<u8> = vec!(0; 128);
    /// for i in 0..8 {
    ///     for j in 0..16 {
    ///         grid[i * 16 + j] = (2 * i) as u8;
    ///     }
    /// }
    /// m.set_all_intensity(&grid);
    /// ```
    pub fn set_all_intensity(&mut self, leds: &[u8]) {
        if self.device_type != MonomeDeviceType::Grid {
            error!("Called grid method on something that is not an grid.");
            return;
        }
        let width_in_quad = self.size.0 / 8;
        let height_in_quad = self.size.1 / 8;
        let width = self.size.0;
        let quad_size = 8;

        let mut quad = [0 as u8; 64];
        for a in 0..height_in_quad {
            for b in 0..width_in_quad {
                // Get the quad into an array
                for i in 0..8 as i32 {
                    for j in 0..8 as i32 {
                        let idx = toidx(b * quad_size + j, a * quad_size + i, width);
                        quad[(i * 8 + j) as usize] = leds[idx];
                    }
                }
                self.map(b * 8, a * 8, &quad);
            }
        }
    }

    /// Set the value an 8x8 quad of led on a monome grid.
    ///
    /// # Arguments
    ///
    /// * `x_offset` - at which offset, that must be a multiple of 8, to set the quad.
    /// * `y_offset` - at which offset, that must be a multiple of 8, to set the quad.
    /// * `masks` - a vector of 8 unsigned 8-bit integers that is a mask representing the leds to
    /// light up, or a vector of 64 bools, true for led On, false for led Off, packed in row order,
    /// or a vector of 64 integers between 0 and 15, for the brightness of each led, packed in
    /// row order.
    ///
    /// # Example
    ///
    /// On a monome 128, draw a triangle in the lower left half of the rightmost half, and a
    /// gradient on the leftmost half.
    /// ```no_run
    /// # extern crate monome;
    /// # use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// let mut v = [0; 64];
    /// for i in 0..64 {
    ///     v[i] = (i / 4) as u8;
    /// }
    /// monome.map(0, 0, &v);
    /// monome.map(8, 0, &[1, 3, 7, 15, 32, 63, 127, 0b11111111]);
    /// ```
    pub fn map<'a, A>(&mut self, x_offset: i32, y_offset: i32, masks: A)
    where
        A: IntoAddrAndArgs<'a, Vec<OscType>> + Sized,
    {
        if self.device_type != MonomeDeviceType::Grid {
            error!("Called grid method on something that is not an grid.");
            return;
        }
        let (frag, mut arg) = masks.as_addr_frag_and_args();

        let mut args = Vec::with_capacity(2 + arg.len());

        args.push(OscType::Int(x_offset));
        args.push(OscType::Int(y_offset));
        args.append(&mut arg);

        self.send(&format!("/grid/led/{}map", frag), args);
    }

    /// Set a full row of a grid, using one or more 8-bit mask(s), or a vector containing booleans
    /// or integer intensity values.
    ///
    /// # Arguments
    ///
    /// * `x_offset` - at which 8 button offset to start setting the leds. This is always 0 for a
    /// 64, and can be 8 for a 128 or 256.
    /// * `y` - which row to set, 0-indexed. This must be lower than the number of rows of the
    /// device.
    /// * `leds` - either the list of masks that determine the pattern to light on for a particular
    /// 8 led long section, or a vector of either int or bool, one element for each led.
    ///
    /// # Example
    ///
    /// On a monome 128, light up every other led of the right half of the 3rd  row:
    ///
    /// ```no_run
    /// # use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// monome.row(8 /* rightmost half */,
    ///            2 /* 3rd row, 0 indexed */,
    ///            &0b01010101u8 /* every other led, 85 in decimal */);
    /// ```
    pub fn row<'a, A>(&mut self, x_offset: i32, y: i32, leds: &A)
    where
        A: IntoAddrAndArgs<'a, Vec<OscType>>,
    {
        if self.device_type != MonomeDeviceType::Grid {
            error!("Called grid method on something that is not an grid.");
            return;
        }
        let (frag, arg) = leds.as_addr_frag_and_args();

        let mut args = Vec::with_capacity((2 + arg.len()) as usize);

        args.push(OscType::Int(x_offset));
        args.push(OscType::Int(y));
        args.append(&mut arg.to_vec());

        self.send(&format!("/grid/led/{}row", frag), args);
    }

    /// Set a full column of a grid, using one or more 8-bit mask(s), or a vector containing
    /// booleans or integer intensity values.
    ///
    /// # Arguments
    ///
    /// * `x` - which column to set 0-indexed. This must be lower than the number of columns of the
    /// device.
    /// * `y_offset` - at which 8 button offset to start setting the leds. This is always 0 for a
    /// 64, and can be 8 for a 128 or 256.
    /// * `leds` - either the list of masks that determine the pattern to light on for a particular
    /// 8 led long section, or a vector of either int or bool, one element for each led.
    ///
    /// # Example
    ///
    /// On a monome 256, light up every other led of the bottom half of the 3rd column from the
    /// right:
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// monome.col(2 /* 3rd column, 0-indexed */,
    ///            8 /* bottom half */,
    ///            &0b01010101u8 /* every other led, 85 in decimal */);
    /// ```
    pub fn col<'a, A>(&mut self, x: i32, y_offset: i32, leds: &A)
    where
        A: IntoAddrAndArgs<'a, Vec<OscType>>,
    {
        if self.device_type != MonomeDeviceType::Grid {
            error!("Called grid method on something that is not an grid.");
            return;
        }
        let (frag, mut arg) = leds.as_addr_frag_and_args();

        let mut args = Vec::with_capacity((2 + arg.len()) as usize);

        args.push(OscType::Int(x));
        args.push(OscType::Int(y_offset));
        args.append(&mut arg);

        self.send(&format!("/grid/led/{}col", frag), args);
    }

    /// Set a single led, with intensity, on an Arc.
    ///
    /// # Arguments
    ///
    /// - `n` - the encoder to set a led on, 0-indexed.
    /// - `index` - which led to set. 0 is the top led, and goes clockwise. This is modulo 64, so
    /// passing in 65 is the second led from the top, going clockwise.
    /// - `intensity` - the intensity of the led 0 being off, 15 full brightness.
    ///
    /// # Example
    ///
    /// On an arc, make a circular gradient on the first encoder:
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// for i in 0..64 {
    ///   monome.set(0, i, i / 4);
    /// }
    /// ```
    pub fn ring_set(&mut self, n: usize, index: u32, intensity: u32) {
        if self.device_type != MonomeDeviceType::Arc {
            error!("Called arc method on something that is not an arc.");
            return;
        }
        let mut args = Vec::with_capacity(3);
        args.push(OscType::Int(n as i32));
        args.push(OscType::Int(index as i32));
        args.push(OscType::Int(intensity as i32));

        self.send("/ring/set", args);
    }

    /// Set all the led on an encoder to a particular intensity.
    ///
    /// # Arguments
    ///
    /// - `n` - the encoder to set the leds on, 0-indexed.
    /// - `intensity` - the intensity of the leds: 0 being off, 15 full brightness.
    ///
    /// # Example
    ///
    /// On an arc, make a gradient accross all four encoders:
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// for i in 0..4 {
    ///   monome.ring_all(i, (i * 4) as u32);
    /// }
    /// ```
    pub fn ring_all(&mut self, n: usize, intensity: u32) {
        if self.device_type != MonomeDeviceType::Arc {
            error!("Called arc method on something that is not an arc.");
            return;
        }
        let mut args = Vec::with_capacity(2);
        args.push(OscType::Int(n as i32));
        args.push(OscType::Int(intensity as i32));

        self.send("/ring/all", args);
    }

    /// Set a range of led to a particular intensity.
    ///
    /// # Arguments
    ///
    /// - `n` - the encoder to set the leds on, 0-indexed.
    /// - `start_offset` - the encoder to start setting the led from, 0-indexed, modulo 64.
    /// - `end_offset` - the encoder to end setting the led at 0-indexed, inclusive, modulo 64.
    /// - `intensity` - the intensity of the leds: 0 being off, 15 full brightness.
    ///
    /// # Example
    ///
    /// On an arc, lit up halves:
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// monome.ring_range(0, 0, 32, 15);
    /// monome.ring_range(1, 32, 64, 15);
    /// monome.ring_range(2, 16, 48, 15);
    /// monome.ring_range(3, 48, 16, 15);
    /// ```
    pub fn ring_range(&mut self, n: usize, start_offset: usize, end_offset: usize, intensity: u32) {
        if self.device_type != MonomeDeviceType::Arc {
            error!("Called arc method on something that is not an arc.");
            return;
        }
        let mut args = Vec::with_capacity(4);
        args.push(OscType::Int(n as i32));
        args.push(OscType::Int(start_offset as i32));
        args.push(OscType::Int(end_offset as i32));
        args.push(OscType::Int(intensity as i32));

        self.send("/ring/range", args);
    }

    /// Set all leds on an encoder to specific values.
    ///
    /// # Arguments
    ///
    /// - `n` - the encoder to set the leds on, 0-indexed.
    /// - `values` - an array of 64 values between 0 an 16, one for each led.
    ///
    /// # Example
    ///
    /// On an arc, make a gradient on an encoder.
    ///
    /// ```no_run
    /// use monome::Monome;
    /// let mut monome = Monome::new("/prefix").unwrap();
    /// let mut v: [u8; 64] = [0; 64];
    ///
    /// for i in 0..64 {
    ///     v[i] = (i / 4) as u8;
    /// }
    /// monome.ring_map(0, &v);
    /// ```
    pub fn ring_map(&mut self, n: usize, values: &[u8; 64]) {
        let mut args = Vec::with_capacity(65);
        args.push(OscType::Int(n as i32));
        for v in values.iter() {
            args.push(OscType::Int(i32::from(*v)));
        }

        self.send("/ring/map", args);
    }

    /// Enable or disable all tilt sensors (usually, there is only one), which allows receiving the
    /// `/<prefix>/tilt/` events, with the n,x,y,z coordinates as parameters.
    pub fn tilt_all(&mut self, on: bool) {
        self.send(
            "/tilt/set",
            vec![OscType::Int(0), OscType::Int(if on { 1 } else { 0 })],
        );
    }

    /// Set the rotation for this device. This is either 0, 90, 180 or 270
    pub fn set_rotation(&mut self, rotation: i32) {
        self.send_no_prefix("/sys/rotation", vec![OscType::Int(rotation)]);
        self.rotation = rotation;
    }

    /// Set the prefix for this device.
    pub fn set_prefix(&mut self, prefix: String) {
        self.send_no_prefix("/sys/prefix", vec![OscType::String(prefix.clone())]);
        self.prefix = prefix;
    }

    /// Get the name of this device.
    pub fn name(&self) -> String {
        self.name.clone()
    }

    /// Get the type for this device (for example `"monome 128"`).
    pub fn device_type(&self) -> MonomeDeviceType {
        self.device_type.clone()
    }

    /// Get the port for this device.
    pub fn port(&self) -> i32 {
        self.port
    }

    /// Get the host for this device is at.
    pub fn host(&self) -> String {
        self.host.clone()
    }

    /// Get the id of this device.
    pub fn id(&self) -> String {
        self.id.clone()
    }

    /// Get the current prefix of this device.
    pub fn prefix(&self) -> String {
        self.prefix.clone()
    }

    /// Get the current rotation of this device.
    pub fn rotation(&self) -> i32 {
        self.rotation
    }

    /// Get the size of this device, as a `(width, height)`.
    pub fn size(&self) -> (i32, i32) {
        self.size
    }
    /// Get the width of this device.
    pub fn width(&self) -> usize {
        self.size.0 as usize
    }

    /// Get the height of this device.
    pub fn height(&self) -> usize {
        self.size.1 as usize
    }

    /// Adds the prefix, packs the OSC message into an u8 vector and sends it to the transport.
    fn send(&mut self, addr: &str, args: Vec<OscType>) {
        let with_prefix = format!("{}{}", self.prefix, addr);
        self.send_no_prefix(&with_prefix, args);
    }

    /// Packs the OSC message into an u8 vector and sends it to the transport.
    fn send_no_prefix(&mut self, addr: &str, args: Vec<OscType>) {
        let message = OscMessage {
            addr: addr.to_owned(),
            args: Some(args),
        };
        let packet = OscPacket::Message(message);
        debug!("⇨ {:?}", packet);
        let bytes: Vec<u8> = encode(&packet).unwrap();
        match self.tx.try_send(bytes) {
            Ok(()) => {}
            Err(b) => {
                let full = b.is_full();
                let disconnected = b.is_disconnected();
                error!("full: {:?}, disconnected: {:?}", full, disconnected);
            }
        }
    }

    /// Receives a MonomeEvent, from a connected monome, which can be a grid key press, an event
    /// from the tilt sensor, or a delta from an encoder, on an Arc. Only the events from the set
    /// `prefix` will be received.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # extern crate monome;
    ///
    /// use monome::{Monome, MonomeEvent, KeyDirection};
    /// let mut m = Monome::new("/prefix").unwrap();
    ///
    /// loop {
    ///     match m.poll() {
    ///         Some(MonomeEvent::GridKey{x, y, direction}) => {
    ///             match direction {
    ///                 KeyDirection::Down => {
    ///                     println!("Key pressed: {}x{}", x, y);
    ///                 }
    ///                 KeyDirection::Up => {
    ///                     println!("Key released: {}x{}", x, y);
    ///                 }
    ///             }
    ///         }
    ///         Some(MonomeEvent::Tilt{n: _n, x, y, z: _z}) => {
    ///           println!("tilt update: pitch: {}, roll {}", x, y);
    ///         }
    ///         _ => {
    ///           break;
    ///         }
    ///     }
    /// }
    /// ```
    pub fn poll(&mut self) -> Option<MonomeEvent> {
        match self.q.pop() {
            Ok(buf) => self.parse(&buf),
            Err(crossbeam::queue::PopError) => {
                None
            }
        }
    }

    fn parse(&self, buf: &[u8]) -> Option<MonomeEvent> {
        let packet = decode(buf).unwrap();
        debug!("⇦ {:?}", packet);

        match packet {
            OscPacket::Message(message) => {
                if message.addr.starts_with("/serialosc") {
                    if message.addr == "/serialosc/device" {
                        info!("/serialosc/device");
                    } else if message.addr == "/serialosc/add" {
                        if let Some(args) = message.args {
                            if let OscType::String(ref device_name) = args[0] {
                                info!("device added: {}", device_name);
                            } else {
                                warn!("unexpected message for prefix {}", message.addr);
                            }
                        } else if message.addr == "/serialosc/remove" {
                            if let Some(args) = message.args {
                                if let OscType::String(ref device_name) = args[0] {
                                    info!("device removed: {}", device_name);
                                } else {
                                    warn!("unexpected message for prefix {}", message.addr);
                                }
                            }
                        };
                    }
                } else if message.addr.starts_with("/sys") {
                    // This should only be received during the setup phase
                    debug!("/sys received: {:?}", message);
                } else if message.addr.starts_with(&self.prefix) {
                    if let Some(args) = &message.args {
                        if message
                            .addr
                            .starts_with(&format!("{}/grid/key", self.prefix))
                        {
                            if let [OscType::Int(x), OscType::Int(y), OscType::Int(v)] =
                                args.as_slice()
                            {
                                info!("Key: {}:{} {}", *x, *y, *v);
                                let direction = if *v == 1 {
                                    KeyDirection::Down
                                } else {
                                    KeyDirection::Up
                                };
                                return Some(MonomeEvent::GridKey {
                                    x: *x,
                                    y: *y,
                                    direction,
                                });
                            }
                            error!("Invalid /grid/key message received {:?}.", message);
                        } else if message.addr.starts_with(&format!("{}/tilt", self.prefix)) {
                            if let [OscType::Int(n), OscType::Int(x), OscType::Int(y), OscType::Int(z)] =
                                args.as_slice()
                            {
                                info!("Tilt {} {},{},{}", *n, *x, *y, *z);
                                return Some(MonomeEvent::Tilt {
                                    n: *n,
                                    x: *x,
                                    y: *y,
                                    z: *z,
                                });
                            }
                            error!("Invalid /tilt message received {:?}.", message);
                        } else if message
                            .addr
                            .starts_with(&format!("{}/enc/delta", self.prefix))
                        {
                            if let [OscType::Int(n), OscType::Int(delta)] = args.as_slice() {
                                info!("Encoder delta {} {}", *n, *delta);
                                return Some(MonomeEvent::EncoderDelta {
                                    n: *n as usize,
                                    delta: *delta,
                                });
                            }
                            error!("Invalid /end/delta message received {:?}.", message);
                        } else if message
                            .addr
                            .starts_with(&format!("{}/enc/key", self.prefix))
                        {
                            if let [OscType::Int(n), OscType::Int(direction)] = args.as_slice() {
                                info!("Encoder key {} {}", *n, *direction);
                                return Some(MonomeEvent::EncoderKey {
                                    n: *n as usize,
                                    direction: if *direction == 1 {
                                        KeyDirection::Down
                                    } else {
                                        KeyDirection::Up
                                    },
                                });
                            }
                            error!("Invalid /end/key message received {:?}.", message);
                        } else {
                            error!("not handled: {:?}", message.addr);
                        }
                    }
                }
                None
            }
            OscPacket::Bundle(_bundle) => {
                panic!("wtf.");
            }
        }
    }
}

impl fmt::Debug for Monome {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let rv = write!(
            f,
            "Monome {}\n\ttype: {}\n\tport: {}\n\thost: {}\n\t\
            id: {}\n\tprefix: {}\n\trotation: {}",
            self.name,
            self.device_type,
            self.port,
            self.host,
            self.id,
            self.prefix,
            self.rotation
        );
        if self.device_type == MonomeDeviceType::Grid {
            return write!(f, "\n\tsize: {}:{}", self.size.0, self.size.1);
        }
        rv
    }
}

impl fmt::Display for Monome {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[cfg(test)]
mod tests {
    use crate::build_osc_message;
    use crate::Monome;
    use crate::SERIALOSC_PORT;
    use rosc::decoder::decode;
    use rosc::encoder::encode;
    use rosc::{OscPacket, OscType};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use tokio::net::UdpSocket;
    use tokio::prelude::*;

    #[test]
    fn setup() {
        let pair = Arc::new((Mutex::new(false), Condvar::new()));
        let pair2 = pair.clone();

        thread::spawn(move || {
            let fake_device_port = 1234;
            let device_addr = format!("127.0.0.1:{}", fake_device_port).parse().unwrap();
            let device_socket = UdpSocket::bind(&device_addr).unwrap();

            // Avoid failing if serialocs is running on the default port.
            let serialosc_addr = format!("127.0.0.1:{}", SERIALOSC_PORT + 1).parse().unwrap();
            let serialosc_socket = UdpSocket::bind(&serialosc_addr).unwrap();

            {
                let &(ref lock, ref cvar) = &*pair2;
                let mut started = lock.lock().unwrap();
                *started = true;
                cvar.notify_all();
            }

            let (socket, data, _, _) = serialosc_socket.recv_dgram(vec![0u8; 1024]).wait().unwrap();
            let packet = decode(&data).unwrap();

            let msg = match packet {
                OscPacket::Message(m) => m,
                OscPacket::Bundle(_b) => panic!("unexpected bundle"),
            };
            assert!(msg.addr == "/serialosc/list");
            assert!(msg.args.is_some());

            let app_port = if let OscType::Int(port) = msg.args.unwrap()[1] {
                port
            } else {
                panic!("bad message");
            };

            let packet = build_osc_message(
                "/serialosc/device",
                vec![
                    OscType::String("monome grid test".into()),
                    OscType::String("m123123".into()),
                    OscType::Int(1234),
                ],
            );

            let bytes: Vec<u8> = encode(&packet).unwrap();

            let app_addr = format!("127.0.0.1:{}", app_port).parse().unwrap();
            let (mut socket, _) = socket.send_dgram(bytes, &app_addr).wait().unwrap();

            fn receive_from_app_and_expect(
                socket: UdpSocket,
                expected_addr: String,
            ) -> (UdpSocket, Option<Vec<OscType>>) {
                let (socket, data, _, _) = socket.recv_dgram(vec![0u8; 1024]).wait().unwrap();
                let packet = decode(&data).unwrap();

                let msg = match packet {
                    OscPacket::Message(m) => m,
                    OscPacket::Bundle(_b) => panic!("unexpected bundle"),
                };

                assert!(msg.addr == expected_addr);

                (socket, msg.args)
            }

            let (device_socket, args) =
                receive_from_app_and_expect(device_socket, "/sys/port".into());
            let port = if let OscType::Int(port) = args.unwrap()[0] {
                assert!(port == 10000);
                port
            } else {
                panic!("bad port");
            };
            assert!(port == 10000);
            let (device_socket, args) =
                receive_from_app_and_expect(device_socket, "/sys/host".into());
            let argss = args.unwrap();
            let host = if let OscType::String(ref host) = argss[0] {
                host
            } else {
                panic!("bad host");
            };
            assert!(host == "127.0.0.1");
            let (device_socket, args) =
                receive_from_app_and_expect(device_socket, "/sys/prefix".into());
            let argss = args.unwrap();
            let prefix = if let OscType::String(ref prefix) = argss[0] {
                prefix
            } else {
                panic!("bad prefix");
            };
            assert!(prefix == "/plop");
            let (_device_socket, args) =
                receive_from_app_and_expect(device_socket, "/sys/info".into());
            assert!(args.is_none());

            let message_addrs = vec![
                "/sys/port",
                "/sys/host",
                "/sys/id",
                "/sys/prefix",
                "/sys/rotation",
                "/sys/size",
            ];

            let message_args = vec![
                vec![OscType::Int(fake_device_port)],
                vec![OscType::String("127.0.0.1".into())],
                vec![OscType::String("monome blabla".into())],
                vec![OscType::String("/plop".into())],
                vec![OscType::Int(0)],
                vec![OscType::Int(16), OscType::Int(8)],
            ];

            assert!(message_addrs.len() == message_args.len());

            for i in 0..message_addrs.len() {
                let packet = build_osc_message(message_addrs[i], message_args[i].clone());
                let bytes: Vec<u8> = encode(&packet).unwrap();
                socket = socket
                    .send_dgram(bytes, &app_addr)
                    .map(|(socket, _)| socket)
                    .wait()
                    .unwrap();
            }
        });

        let &(ref lock, ref cvar) = &*pair;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = cvar.wait(started).unwrap();
        }

        // use another port in case serialosc is running on the local machine
        let _m = Monome::new_with_port("/plop".to_string(), SERIALOSC_PORT + 1).unwrap();
    }
}
