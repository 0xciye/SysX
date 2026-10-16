use image::{ImageFormat, RgbImage};
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
    let decoded =
        image::load_from_memory_with_format(&frame.jpeg, ImageFormat::Jpeg).map_err(|error| {
            ReceiverError::Decode {
                frame_id: frame.frame_id,
                message: error.to_string(),
            }
        })?;
    let image = decoded.to_rgb8();
    if image.width() != u32::from(frame.width) || image.height() != u32::from(frame.height) {
        return Err(ReceiverError::DimensionMismatch {
            frame_id: frame.frame_id,
            declared_width: frame.width,
            declared_height: frame.height,
            actual_width: image.width(),
            actual_height: image.height(),
        });
    }
    Ok(SourceFrame {
        frame_id: frame.frame_id,
        timestamp_ns: frame.timestamp_ns,
        image,
        source: "udp".into(),
        jpeg: Arc::new(frame.jpeg),
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
        self.open()?;
        let deadline = Instant::now() + timeout;
        loop {
            self.reassembler.expire(monotonic_ns());
            if let Some(frame) = self.reassembler.take_latest() {
                match decode_udp_frame(frame) {
                    Ok(frame) => return Ok(Some(frame)),
                    Err(_) => self.reassembler.reject_decoded_frame(),
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            match self
                .socket
                .as_ref()
                .expect("socket opened")
                .recv_from(&mut self.datagram)
            {
                Ok((length, sender)) => {
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
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
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
    fn bad_jpeg_and_dimension_mismatch_do_not_interrupt_the_listener() {
        let mut source = UdpDecodedFrameSource::new("127.0.0.1", 1).unwrap();
        source.port = 0;
        source.open().unwrap();
        let address = source.local_addr().unwrap().unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
        let jpeg = include_bytes!("../../src/assets/red-cube-192.jpg");
        for (id, size, bytes) in [
            (1, 192, b"\xff\xd8invalid\xff\xd9".as_slice()),
            (2, 1, jpeg.as_slice()),
            (3, 192, jpeg.as_slice()),
        ] {
            for packet in crate::udp::encode_jpeg_frame(id, size, size, id, bytes).unwrap() {
                sender.send_to(&packet, address).unwrap();
            }
        }
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
}
