use std::collections::{BTreeMap, HashMap};

pub const MAGIC: &[u8; 4] = b"RCVR";
pub const VERSION: u8 = 1;
pub const HEADER_BYTES: usize = 30;
pub const MAX_DATAGRAM_BYTES: usize = 1200;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_FRAME_PIXELS: u64 = 33_554_432;
pub const MAX_DIMENSION: u16 = 8192;
pub const MAX_IN_FLIGHT_FRAMES: usize = 32;
pub const MAX_REASSEMBLY_BYTES: usize = 128 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_NS: u64 = 100_000_000;
const MAX_CHUNKS: u16 = ((MAX_FRAME_BYTES + MAX_DATAGRAM_BYTES - HEADER_BYTES - 1)
    / (MAX_DATAGRAM_BYTES - HEADER_BYTES)) as u16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    pub frame_id: u64,
    pub width: u16,
    pub height: u16,
    pub timestamp_ns: u64,
    pub jpeg: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReassemblyStats {
    pub dropped_frames: u64,
    pub invalid_packets: u64,
    pub expired_frames: u64,
    pub in_flight_bytes: usize,
}

struct Assembly {
    width: u16,
    height: u16,
    timestamp_ns: u64,
    total: u16,
    created_ns: u64,
    chunks: BTreeMap<u16, Vec<u8>>,
    bytes: usize,
}

pub struct UdpFrameReassembler {
    timeout_ns: u64,
    assemblies: HashMap<u64, Assembly>,
    insertion_order: Vec<u64>,
    ready: Option<EncodedFrame>,
    stats: ReassemblyStats,
}

pub fn encode_jpeg_frame(
    frame_id: u64,
    width: u16,
    height: u16,
    timestamp_ns: u64,
    jpeg: &[u8],
) -> Result<Vec<Vec<u8>>, std::io::Error> {
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid JPEG frame");
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS
        || jpeg.len() < 4
        || jpeg.len() > MAX_FRAME_BYTES
        || !jpeg.starts_with(&[0xff, 0xd8])
        || !jpeg.ends_with(&[0xff, 0xd9])
    {
        return Err(invalid());
    }
    let payload_capacity = MAX_DATAGRAM_BYTES - HEADER_BYTES;
    let total = jpeg.len().div_ceil(payload_capacity);
    if total == 0 || total > usize::from(MAX_CHUNKS) {
        return Err(invalid());
    }
    let total = u16::try_from(total).map_err(|_| invalid())?;
    let mut packets = Vec::with_capacity(usize::from(total));
    for (index, payload) in jpeg.chunks(payload_capacity).enumerate() {
        let mut packet = Vec::with_capacity(HEADER_BYTES + payload.len());
        packet.extend_from_slice(MAGIC);
        packet.extend_from_slice(&[VERSION, 0]);
        packet.extend_from_slice(&frame_id.to_be_bytes());
        packet.extend_from_slice(&u16::try_from(index).map_err(|_| invalid())?.to_be_bytes());
        packet.extend_from_slice(&total.to_be_bytes());
        packet.extend_from_slice(&width.to_be_bytes());
        packet.extend_from_slice(&height.to_be_bytes());
        packet.extend_from_slice(&timestamp_ns.to_be_bytes());
        packet.extend_from_slice(payload);
        packets.push(packet);
    }
    Ok(packets)
}

impl Default for UdpFrameReassembler {
    fn default() -> Self {
        Self::new(DEFAULT_TIMEOUT_NS)
    }
}

impl UdpFrameReassembler {
    pub fn new(timeout_ns: u64) -> Self {
        Self {
            timeout_ns,
            assemblies: HashMap::new(),
            insertion_order: Vec::new(),
            ready: None,
            stats: ReassemblyStats::default(),
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        let mut stats = self.stats.clone();
        stats.in_flight_bytes = self
            .assemblies
            .values()
            .map(|assembly| assembly.bytes)
            .sum();
        stats
    }

    pub fn accept(&mut self, packet: &[u8], now_ns: u64) -> Option<EncodedFrame> {
        self.expire(now_ns);
        if packet.len() <= HEADER_BYTES || packet.len() > MAX_DATAGRAM_BYTES {
            self.stats.invalid_packets += 1;
            return None;
        }
        let Some((frame_id, index, total, width, height, timestamp_ns)) = parse_header(packet)
        else {
            self.stats.invalid_packets += 1;
            return None;
        };
        let payload = &packet[HEADER_BYTES..];
        let payload_capacity = MAX_DATAGRAM_BYTES - HEADER_BYTES;
        if !packet.starts_with(MAGIC)
            || packet[4] != VERSION
            || packet[5] != 0
            || payload.is_empty()
            || total == 0
            || total > MAX_CHUNKS
            || index >= total
            || width == 0
            || height == 0
            || width > MAX_DIMENSION
            || height > MAX_DIMENSION
            || u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS
            || (index < total - 1 && payload.len() != payload_capacity)
        {
            self.stats.invalid_packets += 1;
            return None;
        }
        if (index == 0 && !payload.starts_with(&[0xff, 0xd8]))
            || (index == total - 1 && !payload.ends_with(&[0xff, 0xd9]))
        {
            self.discard(frame_id);
            self.stats.invalid_packets += 1;
            return None;
        }

        if !self.assemblies.contains_key(&frame_id) {
            while self.assemblies.len() >= MAX_IN_FLIGHT_FRAMES {
                let Some(oldest) = self.insertion_order.first().copied() else {
                    break;
                };
                self.discard(oldest);
                self.stats.dropped_frames += 1;
            }
            self.insertion_order.push(frame_id);
            self.assemblies.insert(
                frame_id,
                Assembly {
                    width,
                    height,
                    timestamp_ns,
                    total,
                    created_ns: now_ns,
                    chunks: BTreeMap::new(),
                    bytes: 0,
                },
            );
        }

        let Some(current_assembly) = self.assemblies.get(&frame_id) else {
            return None;
        };
        if (
            current_assembly.width,
            current_assembly.height,
            current_assembly.timestamp_ns,
            current_assembly.total,
        ) != (width, height, timestamp_ns, total)
        {
            self.discard(frame_id);
            self.stats.invalid_packets += 1;
            return None;
        }
        if let Some(previous) = current_assembly.chunks.get(&index) {
            if previous.as_slice() != payload {
                self.discard(frame_id);
                self.stats.invalid_packets += 1;
            }
            return None;
        }
        let current_bytes = current_assembly.bytes;
        let buffered: usize = self.assemblies.values().map(|item| item.bytes).sum();
        if current_bytes.saturating_add(payload.len()) > MAX_FRAME_BYTES
            || buffered.saturating_add(payload.len()) > MAX_REASSEMBLY_BYTES
        {
            self.discard(frame_id);
            self.stats.invalid_packets += 1;
            return None;
        }
        let Some(assembly) = self.assemblies.get_mut(&frame_id) else {
            return None;
        };
        assembly.chunks.insert(index, payload.to_vec());
        assembly.bytes += payload.len();
        if assembly.chunks.len() != usize::from(total) {
            return None;
        }
        let assembly = self.assemblies.remove(&frame_id)?;
        self.insertion_order.retain(|id| *id != frame_id);
        let mut jpeg = Vec::with_capacity(assembly.bytes);
        for (_, chunk) in assembly.chunks {
            jpeg.extend_from_slice(&chunk);
        }
        let frame = EncodedFrame {
            frame_id,
            width,
            height,
            timestamp_ns,
            jpeg,
        };
        if self.ready.replace(frame.clone()).is_some() {
            self.stats.dropped_frames += 1;
        }
        Some(frame)
    }

    pub fn take_latest(&mut self) -> Option<EncodedFrame> {
        self.ready.take()
    }

    pub fn expire(&mut self, now_ns: u64) {
        let expired: Vec<u64> = self
            .assemblies
            .iter()
            .filter_map(|(id, assembly)| {
                (now_ns.saturating_sub(assembly.created_ns) > self.timeout_ns).then_some(*id)
            })
            .collect();
        for frame_id in expired {
            self.discard(frame_id);
            self.stats.expired_frames += 1;
        }
    }

    fn discard(&mut self, frame_id: u64) {
        self.assemblies.remove(&frame_id);
        self.insertion_order.retain(|id| *id != frame_id);
    }
}

fn parse_header(packet: &[u8]) -> Option<(u64, u16, u16, u16, u16, u64)> {
    if packet.len() < HEADER_BYTES {
        return None;
    }
    Some((
        u64::from_be_bytes(packet[6..14].try_into().ok()?),
        u16::from_be_bytes(packet[14..16].try_into().ok()?),
        u16::from_be_bytes(packet[16..18].try_into().ok()?),
        u16::from_be_bytes(packet[18..20].try_into().ok()?),
        u16::from_be_bytes(packet[20..22].try_into().ok()?),
        u64::from_be_bytes(packet[22..30].try_into().ok()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(frame_id: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[VERSION, 0]);
        bytes.extend_from_slice(&frame_id.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&1u16.to_be_bytes());
        bytes.extend_from_slice(&123u64.to_be_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn reassembles_latest_frame_and_rejects_bad_jpeg_boundaries() {
        let mut receiver = UdpFrameReassembler::default();
        assert!(receiver
            .accept(&packet(1, &[0xff, 0xd8, 4, 0xff, 0xd9]), 10)
            .is_some());
        assert_eq!(receiver.take_latest().unwrap().frame_id, 1);
        assert!(receiver.accept(&packet(2, &[1, 2, 3]), 11).is_none());
        assert_eq!(receiver.stats().invalid_packets, 1);
    }

    #[test]
    fn invalid_chunk_metadata_is_counted_without_retaining_payload() {
        let mut receiver = UdpFrameReassembler::default();
        let mut malformed = packet(1, &[0xff, 0xd8, 0xff, 0xd9]);
        malformed[4] = 9;
        receiver.accept(&malformed, 10);
        let stats = receiver.stats();
        assert_eq!(stats.invalid_packets, 1);
        assert_eq!(stats.in_flight_bytes, 0);
    }

    #[test]
    fn jpeg_encoder_round_trips_through_the_reassembler() {
        let jpeg = [0xff, 0xd8, 1, 2, 3, 0xff, 0xd9];
        let packets = encode_jpeg_frame(7, 2, 2, 123, &jpeg).unwrap();
        assert_eq!(packets.len(), 1);
        assert!(packets[0].len() <= MAX_DATAGRAM_BYTES);
        let mut receiver = UdpFrameReassembler::default();
        for packet in packets {
            receiver.accept(&packet, 456);
        }
        assert_eq!(receiver.take_latest().unwrap().jpeg, jpeg);
    }
}
