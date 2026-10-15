use image::{ImageFormat, RgbImage};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};
use thiserror::Error;

use crate::udp::{EncodedFrame, ReassemblyStats, UdpFrameReassembler, MAX_DATAGRAM_BYTES};

#[derive(Clone, Debug)]
pub struct SourceFrame {
    pub frame_id: u64,
    pub timestamp_ns: u64,
    pub image: RgbImage,
    pub source: String,
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
}

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
            let socket = UdpSocket::bind((self.host.as_str(), self.port))?;
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
                return decode_udp_frame(frame).map(Some);
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
                    if self.sender != Some(sender) {
                        self.sender = Some(sender);
                        self.send_crop_config();
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
    }

    fn send_crop_config(&self) {
        let (Some(socket), Some(sender)) = (&self.socket, self.sender) else {
            return;
        };
        let message = format!("SYSCFG 1 CROP {}", self.crop_resolution);
        let _ = socket.send_to(message.as_bytes(), sender);
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
