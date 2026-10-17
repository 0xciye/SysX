use image::RgbImage;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

use crate::udp::{EncodedFrame, ReassemblyStats, UdpFrameReassembler, MAX_DATAGRAM_BYTES};

#[derive(Clone, Debug)]
pub struct SourceFrame {
    pub frame_id: u64,
    pub timestamp_ns: u64,
    pub image: RgbImage,
    pub source: String,
    /// The sender's original JPEG, forwarded to Preview as-is so the monitor
    /// can show every input frame without re-encoding. Empty when unknown.
    pub jpeg: Arc<Vec<u8>>,
    /// When the receiver had the complete frame; movement measures the
    /// frame's age from here, so decode and processing time count too.
    pub received: Instant,
}

#[derive(Debug, Error)]
pub enum ReceiverError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("frame {frame_id} is not a complete JPEG frame")]
    InvalidJpegBoundary { frame_id: u64 },
    #[error("frame {frame_id} contains invalid JPEG data: {message}")]
    Decode { frame_id: u64, message: String },
    #[error("frame {frame_id} declares {declared_width}x{declared_height} but JPEG decodes as {actual_width}x{actual_height}")]
    DimensionMismatch {
        frame_id: u64,
        declared_width: u16,
        declared_height: u16,
        actual_width: u32,
        actual_height: u32,
    },
}

pub fn decode_udp_frame(frame: EncodedFrame) -> Result<SourceFrame, ReceiverError> {
    if frame.jpeg.len() < 4
        || !frame.jpeg.starts_with(&[0xff, 0xd8])
        || !frame.jpeg.ends_with(&[0xff, 0xd9])
    {
        return Err(ReceiverError::InvalidJpegBoundary {
            frame_id: frame.frame_id,
        });
    }
    let (width, height) = (u32::from(frame.width), u32::from(frame.height));
    let pixels =
        crate::jpeg::decode(&frame.jpeg, width, height, false).map_err(|error| match error {
            crate::jpeg::JpegError::Dimensions {
                width: actual_width,
                height: actual_height,
            } => ReceiverError::DimensionMismatch {
                frame_id: frame.frame_id,
                declared_width: frame.width,
                declared_height: frame.height,
                actual_width,
                actual_height,
            },
            crate::jpeg::JpegError::Decode(message) => ReceiverError::Decode {
                frame_id: frame.frame_id,
                message,
            },
        })?;
    let image = RgbImage::from_raw(width, height, pixels).expect("decoded RGB size");
    Ok(SourceFrame {
        frame_id: frame.frame_id,
        timestamp_ns: frame.timestamp_ns,
        image,
        source: "udp".into(),
        jpeg: Arc::new(frame.jpeg),
        received: Instant::now(),
    })
}

pub struct UdpDecodedFrameSource {
    host: String,
    port: u16,
    socket: Option<UdpSocket>,
    reassembler: UdpFrameReassembler,
    datagram: [u8; MAX_DATAGRAM_BYTES + 1],
    crop_resolution: u32,
    sender: Option<SocketAddr>,
    config_sent_at: Option<Instant>,
}

/// SYSCFG is unacknowledged UDP; over a LAN/router a lost reply would leave the
/// sender on a stale crop, so it is repeated while the sender keeps streaming.
const CROP_CONFIG_RESEND: Duration = Duration::from_secs(1);
const RECEIVE_BUFFER_BYTES: usize = 8 * 1024 * 1024;
/// More than a full receive buffer of datagrams.
const MAX_DRAINED_DATAGRAMS: usize = 16_384;

impl UdpDecodedFrameSource {
    pub fn new(host: impl Into<String>, port: u16) -> io::Result<Self> {
        if port == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UDP port must be 1..65535",
            ));
        }
        Ok(Self {
            host: host.into(),
            port,
            socket: None,
            reassembler: UdpFrameReassembler::default(),
            datagram: [0; MAX_DATAGRAM_BYTES + 1],
            crop_resolution: 640,
            sender: None,
            config_sent_at: None,
        })
    }

    pub fn set_crop_resolution(&mut self, resolution: u32) {
        if (1..=8192).contains(&resolution) {
            self.crop_resolution = resolution;
            self.send_crop_config();
        }
    }
    pub fn open(&mut self) -> io::Result<()> {
        if self.socket.is_none() {
            let address = (self.host.as_str(), self.port)
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "no UDP bind address")
                })?;
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(address),
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            // Winsock otherwise permits a wildcard listener and a listener
            // on a specific interface to claim the same port simultaneously.
            // Reserve this unicast receiver exclusively before binding.
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawSocket;
                use windows_sys::Win32::Networking::WinSock::{
                    setsockopt, WSAGetLastError, SOCKET_ERROR, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
                };
                let exclusive: i32 = 1;
                let result = unsafe {
                    setsockopt(
                        socket.as_raw_socket() as usize,
                        SOL_SOCKET,
                        SO_EXCLUSIVEADDRUSE,
                        (&exclusive as *const i32).cast(),
                        std::mem::size_of_val(&exclusive) as i32,
                    )
                };
                if result == SOCKET_ERROR {
                    return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
                }
            }
            // Over a LAN a whole frame (~100 datagrams) arrives in a burst
            // while this thread may still be decoding the previous JPEG; the
            // small default Windows buffer would drop chunks and whole frames.
            let _ = socket.set_recv_buffer_size(RECEIVE_BUFFER_BYTES);
            socket.bind(&address.into())?;
            let socket: UdpSocket = socket.into();
            socket.set_read_timeout(Some(Duration::from_millis(50)))?;
            self.socket = Some(socket);
        }
        Ok(())
    }
    pub fn local_addr(&self) -> io::Result<Option<std::net::SocketAddr>> {
        self.socket.as_ref().map(UdpSocket::local_addr).transpose()
    }
    pub fn stats(&self) -> ReassemblyStats {
        self.reassembler.stats()
    }
    pub fn read(&mut self, timeout: Duration) -> Result<Option<SourceFrame>, ReceiverError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let Some(frame) = self.read_encoded(remaining)? else {
                return Ok(None);
            };
            let received = Instant::now();
            match decode_udp_frame(frame) {
                Ok(mut frame) => {
                    frame.received = received;
                    return Ok(Some(frame));
                }
                Err(_) => self.reassembler.reject_decoded_frame(),
            }
        }
    }

    /// The newest complete frame, not yet decoded (the service decodes in a
    /// [`crate::decode_pool::DecodePool`]).
    pub fn read_encoded(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<EncodedFrame>, ReceiverError> {
        self.open()?;
        let deadline = Instant::now() + timeout;
        loop {
            self.reassembler.expire(monotonic_ns());
            // Read everything already queued first, so only the newest
            // complete frame is handed on: when decoding falls behind, the
            // socket buffer (8 MiB) would otherwise hold seconds of old frames
            // and every frame handed on would be one of them.
            self.drain()?;
            if let Some(frame) = self.reassembler.take_latest() {
                return Ok(Some(frame));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            let socket = self.socket.as_ref().expect("socket opened");
            match socket.recv_from(&mut self.datagram) {
                Ok((length, sender)) => self.accept(length, sender),
                Err(error) if quiet(&error) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Accepts every datagram already queued without waiting.
    fn drain(&mut self) -> Result<(), ReceiverError> {
        let socket = self.socket.as_ref().expect("socket opened");
        socket.set_nonblocking(true)?;
        let mut result = Ok(());
        // Bounded so a flood cannot keep this loop from decoding.
        for _ in 0..MAX_DRAINED_DATAGRAMS {
            let socket = self.socket.as_ref().expect("socket opened");
            match socket.recv_from(&mut self.datagram) {
                Ok((length, sender)) => self.accept(length, sender),
                Err(error) if quiet(&error) => break,
                Err(error) => {
                    result = Err(error.into());
                    break;
                }
            }
        }
        self.socket
            .as_ref()
            .expect("socket opened")
            .set_nonblocking(false)?;
        result
    }

    fn accept(&mut self, length: usize, sender: std::net::SocketAddr) {
        // Only a real RCVR sender may become the crop recipient;
        // stray LAN datagrams must not redirect SYSCFG.
        if self.datagram[..length].starts_with(b"RCVR") {
            if self.sender != Some(sender) {
                self.reassembler.reset_session();
            }
            let resend_due = self
                .config_sent_at
                .is_none_or(|sent| sent.elapsed() >= CROP_CONFIG_RESEND);
            if self.sender != Some(sender) || resend_due {
                self.sender = Some(sender);
                self.send_crop_config();
            }
        }
        self.reassembler
            .accept(&self.datagram[..length], monotonic_ns());
    }
    pub fn close(&mut self) {
        self.socket = None;
        self.sender = None;
        self.config_sent_at = None;
        self.reassembler.reset_session();
    }

    fn send_crop_config(&mut self) {
        let (Some(socket), Some(sender)) = (&self.socket, self.sender) else {
            return;
        };
        let message = format!("SYSCFG 1 CROP {}", self.crop_resolution);
        let _ = socket.send_to(message.as_bytes(), sender);
        self.config_sent_at = Some(Instant::now());
    }
}

/// Errors that leave the socket usable. Windows reports an ICMP "port
/// unreachable" for an earlier SYSCFG reply (sender restarting, LAN hiccup)
/// as `ConnectionReset` on the next receive; treating that as fatal closed
/// the listener and stopped frames for the 3 s retry.
fn quiet(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::ConnectionReset
    )
}

fn monotonic_ns() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_listener_cannot_share_a_bound_loopback_port() {
        let busy = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let mut source = UdpDecodedFrameSource::new("0.0.0.0", port).unwrap();
        assert!(
            source.open().is_err(),
            "an occupied interface must block wildcard binding"
        );
        drop(busy);
        source.open().unwrap();
        assert!(
            UdpSocket::bind(("127.0.0.1", port)).is_err(),
            "the wildcard receiver must own its port"
        );
    }

    #[test]
    fn bad_jpeg_and_dimension_mismatch_do_not_interrupt_the_listener() {
        let mut source = UdpDecodedFrameSource::new("127.0.0.1", 1).unwrap();
        source.port = 0;
        source.open().unwrap();
        let address = source.local_addr().unwrap().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        let send = |id: u64, size: u16, bytes: &[u8]| {
            for packet in crate::udp::encode_jpeg_frame(id, size, size, id, bytes).unwrap() {
                sender.send_to(&packet, address).unwrap();
            }
        };
        // One at a time: queued together, the newer frame supersedes the
        // older ones before they are decoded.
        send(1, 192, b"\xff\xd8invalid\xff\xd9");
        assert!(source.read(Duration::from_millis(100)).unwrap().is_none());
        send(2, 1, jpeg);
        assert!(source.read(Duration::from_millis(100)).unwrap().is_none());
        send(3, 192, jpeg);
        let frame = source
            .read(Duration::from_millis(500))
            .unwrap()
            .expect("valid frame after two corrupt frames");
        assert_eq!(frame.frame_id, 3);
        assert_eq!(source.local_addr().unwrap(), Some(address));
        assert_eq!(source.stats().invalid_frames, 2);
        assert_eq!(source.stats().dropped_frames, 2);
        assert!(
            UdpSocket::bind(address).is_err(),
            "listener must remain bound"
        );
    }

    #[test]
    fn a_new_sender_endpoint_can_restart_its_clock_and_frame_counter() {
        let mut source = UdpDecodedFrameSource::new("127.0.0.1", 1).unwrap();
        source.port = 0;
        source.open().unwrap();
        let address = source.local_addr().unwrap().unwrap();
        let old_sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let new_sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        for (sender, id) in [(&old_sender, 100), (&new_sender, 1)] {
            for packet in crate::udp::encode_jpeg_frame(id, 192, 192, id, jpeg).unwrap() {
                sender.send_to(&packet, address).unwrap();
            }
            assert_eq!(
                source
                    .read(Duration::from_millis(500))
                    .unwrap()
                    .unwrap()
                    .frame_id,
                id
            );
        }
    }

    #[test]
    fn decoded_frame_keeps_the_sender_jpeg_for_preview() {
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg").to_vec();
        let frame = decode_udp_frame(EncodedFrame {
            frame_id: 3,
            width: 192,
            height: 192,
            timestamp_ns: 9,
            jpeg: jpeg.clone(),
        })
        .expect("fixture decodes");
        assert_eq!((frame.image.width(), frame.image.height()), (192, 192));
        assert_eq!(frame.jpeg.as_slice(), jpeg.as_slice());
    }

    #[test]
    fn crop_config_is_repeated_to_the_rcvr_sender_and_never_to_stray_hosts() {
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg").to_vec();
        let mut source = UdpDecodedFrameSource::new("127.0.0.1", 1).unwrap();
        source.port = 0; // ephemeral for the test
        source.open().unwrap();
        source.set_crop_resolution(256);
        let target = source.local_addr().unwrap().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        sender
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let stray = UdpSocket::bind("127.0.0.1:0").unwrap();
        stray
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut reply = [0u8; 64];
        let send_frame = |frame_id: u64| {
            for packet in crate::udp::encode_jpeg_frame(frame_id, 192, 192, 0, &jpeg).unwrap() {
                sender.send_to(&packet, target).unwrap();
            }
        };

        stray.send_to(b"hello", target).unwrap();
        send_frame(1);
        let _ = source.read(Duration::from_millis(300)).unwrap();
        let length = sender.recv(&mut reply).expect("first SYSCFG");
        assert_eq!(&reply[..length], b"SYSCFG 1 CROP 256");
        assert!(stray.recv(&mut reply).is_err(), "stray host got SYSCFG");

        // Within the resend interval: no repeat.
        send_frame(2);
        let _ = source.read(Duration::from_millis(300)).unwrap();
        assert!(sender.recv(&mut reply).is_err());

        // After the interval the still-streaming sender gets it again.
        std::thread::sleep(CROP_CONFIG_RESEND);
        send_frame(3);
        let _ = source.read(Duration::from_millis(300)).unwrap();
        let length = sender.recv(&mut reply).expect("repeated SYSCFG");
        assert_eq!(&reply[..length], b"SYSCFG 1 CROP 256");
    }

    #[test]
    fn a_backlog_hands_on_only_the_newest_frame() {
        let mut source = UdpDecodedFrameSource::new("127.0.0.1", 1).unwrap();
        source.port = 0;
        source.open().unwrap();
        let address = source.local_addr().unwrap().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        // Frames that queued up while the receiver was busy decoding.
        for id in 1..=20u64 {
            for packet in crate::udp::encode_jpeg_frame(id, 192, 192, id, jpeg).unwrap() {
                sender.send_to(&packet, address).unwrap();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
        let frame = source
            .read(Duration::from_millis(500))
            .unwrap()
            .expect("newest frame");
        assert_eq!(frame.frame_id, 20);
        assert!(source.read(Duration::from_millis(50)).unwrap().is_none());
    }

    #[test]
    fn libjpeg_turbo_decodes_the_fixture_like_the_reference_decoder() {
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        let ours = crate::jpeg::decode(jpeg, 192, 192, false).unwrap();
        let reference = image::load_from_memory(jpeg)
            .unwrap()
            .into_rgb8()
            .into_raw();
        assert_eq!(ours.len(), reference.len());
        let worst = ours
            .iter()
            .zip(&reference)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 4, "largest channel difference {worst}");
        assert!(matches!(
            crate::jpeg::decode(jpeg, 64, 64, false),
            Err(crate::jpeg::JpegError::Dimensions {
                width: 192,
                height: 192
            })
        ));
        assert!(crate::jpeg::decode(b"\xff\xd8junk\xff\xd9", 192, 192, false).is_err());
    }
}
