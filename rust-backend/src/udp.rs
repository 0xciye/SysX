use std::collections::HashMap;

pub const MAGIC: &[u8; 4] = b"RCVR";
pub const VERSION: u8 = 1;
pub const HEADER_BYTES: usize = 30;
pub const MAX_DATAGRAM_BYTES: usize = 1200;
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_FRAME_PIXELS: u64 = crate::jpeg::MAX_PIXELS;
pub const MAX_DIMENSION: u16 = 8192;
pub const MAX_IN_FLIGHT_FRAMES: usize = 32;
pub const MAX_REASSEMBLY_BYTES: usize = 128 * 1024 * 1024;
pub const DEFAULT_TIMEOUT_NS: u64 = 100_000_000;
/// UDP v1 has no session identifier. A quiet sender may restart its clock;
/// a changed endpoint also explicitly starts a new session in receiver.rs.
const SESSION_IDLE_NS: u64 = 1_000_000_000;
const MAX_CHUNKS: u16 = ((MAX_FRAME_BYTES + MAX_DATAGRAM_BYTES - HEADER_BYTES - 1)
    / (MAX_DATAGRAM_BYTES - HEADER_BYTES)) as u16;

fn projected_reassembly_bytes(
    in_flight: usize,
    ready: usize,
    additional: usize,
    copied_buffered_chunks: usize,
) -> usize {
    in_flight
        .saturating_add(ready)
        .saturating_add(additional)
        .saturating_add(copied_buffered_chunks)
}

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
    pub invalid_frames: u64,
}

struct Assembly {
    width: u16,
    height: u16,
    timestamp_ns: u64,
    total: u16,
    created_ns: u64,
    /// Contiguous prefix, moved directly to the completed frame.
    jpeg: Vec<u8>,
    next_index: u16,
    /// Only chunks received beyond the contiguous prefix need separate buffers.
    chunks: HashMap<u16, Vec<u8>>,
    bytes: usize,
}

pub struct UdpFrameReassembler {
    timeout_ns: u64,
    assemblies: HashMap<u64, Assembly>,
    insertion_order: Vec<u64>,
    ready: Option<EncodedFrame>,
    in_flight_bytes: usize,
    stats: ReassemblyStats,
    last_completed: Option<(u64, u64)>,
    last_packet_ns: Option<u64>,
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
            in_flight_bytes: 0,
            stats: ReassemblyStats::default(),
            last_completed: None,
            last_packet_ns: None,
        }
    }

    pub fn stats(&self) -> ReassemblyStats {
        let mut stats = self.stats.clone();
        stats.in_flight_bytes = self.in_flight_bytes;
        stats
    }

    /// Accepts a datagram; returns true only when it completes a frame.
    /// Completed frames are retrieved through `take_latest` to avoid copying
    /// large JPEG buffers solely for a completion notification.
    pub fn accept(&mut self, packet: &[u8], now_ns: u64) -> bool {
        self.accept_inner(packet, now_ns, true)
    }

    /// Used only while draining a bounded burst; the caller expires once
    /// before and after the burst instead of scanning the assemblies per packet.
    pub(crate) fn accept_unexpired(&mut self, packet: &[u8], now_ns: u64) -> bool {
        self.accept_inner(packet, now_ns, false)
    }

    fn accept_inner(&mut self, packet: &[u8], now_ns: u64, expire: bool) -> bool {
        if expire {
            self.expire(now_ns);
        }
        if packet.len() <= HEADER_BYTES || packet.len() > MAX_DATAGRAM_BYTES {
            self.stats.invalid_packets += 1;
            return false;
        }
        let Some((frame_id, index, total, width, height, timestamp_ns)) = parse_header(packet)
        else {
            self.stats.invalid_packets += 1;
            return false;
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
            return false;
        }
        if (index == 0 && !payload.starts_with(&[0xff, 0xd8]))
            || (index == total - 1 && !payload.ends_with(&[0xff, 0xd9]))
        {
            self.discard(frame_id);
            self.stats.invalid_packets += 1;
            return false;
        }

        if self
            .last_packet_ns
            .is_some_and(|last| now_ns.saturating_sub(last) >= SESSION_IDLE_NS)
        {
            self.reset_session();
        }
        self.last_packet_ns = Some(now_ns);
        // Compare capture time first: OBS can restart its frame counter while
        // its monotonic clock keeps increasing. Frame IDs break timestamp ties.
        if self
            .last_completed
            .is_some_and(|last| (timestamp_ns, frame_id) <= last)
        {
            self.discard(frame_id);
            return false;
        }

        if !self.assemblies.contains_key(&frame_id) {
            if self.assemblies.len() < MAX_IN_FLIGHT_FRAMES
                && (self.assemblies.try_reserve(1).is_err()
                    || self.insertion_order.try_reserve(1).is_err())
            {
                self.stats.dropped_frames += 1;
                return false;
            }
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
                    jpeg: Vec::new(),
                    next_index: 0,
                    chunks: HashMap::new(),
                    bytes: 0,
                },
            );
        }

        let Some(current_assembly) = self.assemblies.get(&frame_id) else {
            return false;
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
            return false;
        }
        if index < current_assembly.next_index {
            let offset = usize::from(index) * payload_capacity;
            if current_assembly.jpeg.get(offset..offset + payload.len()) != Some(payload) {
                self.discard(frame_id);
                self.stats.invalid_packets += 1;
            }
            return false;
        }
        if let Some(previous) = current_assembly.chunks.get(&index) {
            if previous.as_slice() != payload {
                self.discard(frame_id);
                self.stats.invalid_packets += 1;
            }
            return false;
        }
        let current_bytes = current_assembly.bytes;
        let in_order = index == current_assembly.next_index;
        let mut append_bytes = payload.len();
        let mut copied_buffered_chunks = 0usize;
        if in_order {
            let mut next = current_assembly.next_index + 1;
            while let Some(chunk) = current_assembly.chunks.get(&next) {
                append_bytes = append_bytes.saturating_add(chunk.len());
                copied_buffered_chunks = copied_buffered_chunks.saturating_add(chunk.len());
                next += 1;
            }
        }
        let ready_bytes = self.ready.as_ref().map_or(0, |frame| frame.jpeg.len());
        if current_bytes.saturating_add(payload.len()) > MAX_FRAME_BYTES {
            self.discard(frame_id);
            self.stats.invalid_packets += 1;
            return false;
        }
        if projected_reassembly_bytes(
            self.in_flight_bytes,
            ready_bytes,
            payload.len(),
            copied_buffered_chunks,
        ) > MAX_REASSEMBLY_BYTES
        {
            self.discard(frame_id);
            self.stats.dropped_frames += 1;
            return false;
        }
        let reserve_failed = self.assemblies.get_mut(&frame_id).is_none_or(|assembly| {
            if in_order {
                assembly.jpeg.try_reserve(append_bytes).is_err()
            } else {
                assembly.chunks.try_reserve(1).is_err()
            }
        });
        if reserve_failed {
            self.discard(frame_id);
            self.stats.dropped_frames += 1;
            return false;
        }
        let mut chunk = None;
        if !in_order {
            let mut bytes = Vec::new();
            if bytes.try_reserve_exact(payload.len()).is_err() {
                self.discard(frame_id);
                self.stats.dropped_frames += 1;
                return false;
            }
            bytes.extend_from_slice(payload);
            chunk = Some(bytes);
        }
        let (complete, expired) = {
            let Some(assembly) = self.assemblies.get_mut(&frame_id) else {
                return false;
            };
            if let Some(chunk) = chunk {
                assembly.chunks.insert(index, chunk);
            } else {
                assembly.jpeg.extend_from_slice(payload);
                assembly.next_index += 1;
                while let Some(chunk) = assembly.chunks.remove(&assembly.next_index) {
                    assembly.jpeg.extend_from_slice(&chunk);
                    assembly.next_index += 1;
                }
            }
            assembly.bytes += payload.len();
            self.in_flight_bytes += payload.len();
            (
                assembly.next_index == total && assembly.chunks.is_empty(),
                now_ns.saturating_sub(assembly.created_ns) > self.timeout_ns,
            )
        };
        if !complete {
            return false;
        }
        if expired {
            self.discard(frame_id);
            self.stats.expired_frames += 1;
            return false;
        }
        let Some(assembly) = self.assemblies.remove(&frame_id) else {
            return false;
        };
        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(assembly.bytes);
        self.insertion_order.retain(|id| *id != frame_id);
        let frame = EncodedFrame {
            frame_id,
            width,
            height,
            timestamp_ns,
            jpeg: assembly.jpeg,
        };
        self.last_completed = Some((timestamp_ns, frame_id));
        if self.ready.replace(frame).is_some() {
            self.stats.dropped_frames += 1;
        }
        true
    }

    pub fn take_latest(&mut self) -> Option<EncodedFrame> {
        self.ready.take()
    }

    pub fn reject_decoded_frame(&mut self) {
        self.stats.invalid_frames += 1;
        self.stats.dropped_frames += 1;
    }

    pub fn reset_session(&mut self) {
        self.stats.dropped_frames += self.assemblies.len() as u64 + u64::from(self.ready.is_some());
        self.assemblies.clear();
        self.insertion_order.clear();
        self.ready = None;
        self.in_flight_bytes = 0;
        self.last_completed = None;
        self.last_packet_ns = None;
    }

    pub fn expire(&mut self, now_ns: u64) {
        let timeout_ns = self.timeout_ns;
        let mut expired_count = 0;
        let mut expired_bytes = 0;
        self.assemblies.retain(|_, assembly| {
            let expired = now_ns.saturating_sub(assembly.created_ns) > timeout_ns;
            if expired {
                expired_count += 1;
                expired_bytes += assembly.bytes;
            }
            !expired
        });
        if expired_count != 0 {
            self.in_flight_bytes = self.in_flight_bytes.saturating_sub(expired_bytes);
            self.insertion_order
                .retain(|frame_id| self.assemblies.contains_key(frame_id));
            self.stats.expired_frames += expired_count;
        }
    }

    fn discard(&mut self, frame_id: u64) {
        if let Some(assembly) = self.assemblies.remove(&frame_id) {
            self.in_flight_bytes = self.in_flight_bytes.saturating_sub(assembly.bytes);
        }
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

    #[test]
    fn reassembly_budget_includes_ready_and_in_flight_jpegs() {
        assert_eq!(projected_reassembly_bytes(40, 20, 5, 0), 65);
        assert_eq!(projected_reassembly_bytes(40, 20, 5, 10), 75);
        assert!(
            projected_reassembly_bytes(
                MAX_REASSEMBLY_BYTES / 2,
                MAX_REASSEMBLY_BYTES / 4,
                MAX_REASSEMBLY_BYTES / 4 + 1,
                0,
            ) > MAX_REASSEMBLY_BYTES
        );
    }

    #[test]
    fn out_of_order_chunks_join_once_and_identical_duplicates_are_ignored() {
        let payload = MAX_DATAGRAM_BYTES - HEADER_BYTES;
        let mut jpeg = vec![0; payload * 2 + 20];
        jpeg[..2].copy_from_slice(&[0xff, 0xd8]);
        let last = jpeg.len() - 2;
        jpeg[last..].copy_from_slice(&[0xff, 0xd9]);
        let packets = encode_jpeg_frame(7, 2, 2, 123, &jpeg).unwrap();
        assert_eq!(packets.len(), 3);
        let mut receiver = UdpFrameReassembler::default();

        assert!(!receiver.accept(&packets[2], 1));
        assert!(!receiver.accept(&packets[1], 2));
        assert!(!receiver.accept(&packets[1], 3));
        assert!(receiver.accept(&packets[0], 4));
        assert!(!receiver.accept(&packets[0], 5));
        assert_eq!(receiver.stats().invalid_packets, 0);
        assert_eq!(receiver.stats().in_flight_bytes, 0);
        assert_eq!(receiver.take_latest().unwrap().jpeg, jpeg);
    }

    #[test]
    fn a_conflicting_duplicate_in_the_contiguous_prefix_discards_the_frame() {
        let payload = MAX_DATAGRAM_BYTES - HEADER_BYTES;
        let mut jpeg = vec![0; payload + 20];
        jpeg[..2].copy_from_slice(&[0xff, 0xd8]);
        let last = jpeg.len() - 2;
        jpeg[last..].copy_from_slice(&[0xff, 0xd9]);
        let packets = encode_jpeg_frame(8, 2, 2, 124, &jpeg).unwrap();
        let mut receiver = UdpFrameReassembler::default();
        assert!(!receiver.accept(&packets[0], 1));

        let mut conflicting = packets[0].clone();
        conflicting[HEADER_BYTES + 5] ^= 1;
        assert!(!receiver.accept(&conflicting, 2));

        assert_eq!(receiver.stats().invalid_packets, 1);
        assert_eq!(receiver.stats().in_flight_bytes, 0);
        assert!(receiver.assemblies.is_empty());
    }

    #[test]
    fn late_old_frames_and_duplicates_cannot_replace_or_follow_the_newest() {
        let mut receiver = UdpFrameReassembler::default();
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        let newer = encode_jpeg_frame(2, 1, 1, 20, &jpeg).unwrap();
        let older = encode_jpeg_frame(1, 1, 1, 10, &jpeg).unwrap();
        receiver.accept(&newer[0], 1);
        assert!(!receiver.accept(&older[0], 2));
        assert_eq!(receiver.take_latest().unwrap().frame_id, 2);
        assert!(!receiver.accept(&older[0], 3));
        assert!(!receiver.accept(&newer[0], 4));
        assert!(receiver.take_latest().is_none());
    }

    #[test]
    fn chunk_completion_order_cannot_regress_the_frame_stream() {
        let mut receiver = UdpFrameReassembler::default();
        let mut jpeg = vec![0; 2000];
        jpeg[..2].copy_from_slice(&[0xff, 0xd8]);
        jpeg[1998..].copy_from_slice(&[0xff, 0xd9]);
        let old = encode_jpeg_frame(1, 1, 1, 10, &jpeg).unwrap();
        let new = encode_jpeg_frame(2, 1, 1, 20, &jpeg).unwrap();
        receiver.accept(&old[0], 1);
        receiver.accept(&new[1], 2);
        receiver.accept(&new[0], 3);
        assert_eq!(receiver.take_latest().unwrap().frame_id, 2);
        assert!(!receiver.accept(&old[1], 4));
        assert!(receiver.take_latest().is_none());
    }

    #[test]
    fn sender_restarts_work_with_new_capture_time_or_an_idle_session() {
        let mut receiver = UdpFrameReassembler::default();
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        for (id, stamp, now) in [(50, 100, 1), (1, 101, 2), (1, 1, SESSION_IDLE_NS + 2)] {
            let packets = encode_jpeg_frame(id, 1, 1, stamp, &jpeg).unwrap();
            assert!(receiver.accept(&packets[0], now));
            assert_eq!(receiver.take_latest().unwrap().frame_id, id);
        }
    }

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
        assert!(receiver.accept(&packet(1, &[0xff, 0xd8, 4, 0xff, 0xd9]), 10));
        assert_eq!(receiver.take_latest().unwrap().frame_id, 1);
        assert!(!receiver.accept(&packet(2, &[1, 2, 3]), 11));
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
    fn expiring_partial_frames_releases_bytes_and_insertion_order() {
        let mut jpeg = vec![0; MAX_DATAGRAM_BYTES - HEADER_BYTES + 2];
        jpeg[..2].copy_from_slice(&[0xff, 0xd8]);
        let last = jpeg.len() - 2;
        jpeg[last..].copy_from_slice(&[0xff, 0xd9]);
        let packets = encode_jpeg_frame(1, 2, 2, 1, &jpeg).unwrap();
        let mut receiver = UdpFrameReassembler::new(10);

        assert!(!receiver.accept(&packets[0], 1));
        assert_eq!(
            receiver.stats().in_flight_bytes,
            packets[0].len() - HEADER_BYTES
        );
        receiver.expire(12);

        let stats = receiver.stats();
        assert_eq!(stats.in_flight_bytes, 0);
        assert_eq!(stats.expired_frames, 1);
        assert!(receiver.insertion_order.is_empty());
    }

    #[test]
    fn burst_accept_defers_expiry_but_does_not_complete_a_stale_frame() {
        let mut jpeg = vec![0; MAX_DATAGRAM_BYTES - HEADER_BYTES + 2];
        jpeg[..2].copy_from_slice(&[0xff, 0xd8]);
        let last = jpeg.len() - 2;
        jpeg[last..].copy_from_slice(&[0xff, 0xd9]);
        let partial = encode_jpeg_frame(1, 2, 2, 1, &jpeg).unwrap();
        let mut receiver = UdpFrameReassembler::new(10);

        assert!(!receiver.accept(&partial[0], 1));
        let complete = packet(2, &[0xff, 0xd8, 0xff, 0xd9]);
        assert!(receiver.accept_unexpired(&complete, 12));
        assert_eq!(receiver.stats().expired_frames, 0);
        receiver.expire(12);
        assert_eq!(receiver.stats().expired_frames, 1);
        assert_eq!(receiver.take_latest().unwrap().frame_id, 2);
    }

    #[test]
    fn jpeg_encoder_round_trips_through_the_reassembler() {
        let mut jpeg = vec![0; MAX_DATAGRAM_BYTES - HEADER_BYTES + 2];
        jpeg[..2].copy_from_slice(&[0xff, 0xd8]);
        let last = jpeg.len() - 2;
        jpeg[last..].copy_from_slice(&[0xff, 0xd9]);
        let packets = encode_jpeg_frame(7, 2, 2, 123, &jpeg).unwrap();
        assert_eq!(packets.len(), 2);
        assert!(packets[0].len() <= MAX_DATAGRAM_BYTES);
        let mut receiver = UdpFrameReassembler::default();
        assert!(!receiver.accept(&packets[0], 456));
        assert_eq!(
            receiver.stats().in_flight_bytes,
            packets[0].len() - HEADER_BYTES
        );
        assert!(receiver.accept(&packets[1], 457));
        assert_eq!(receiver.stats().in_flight_bytes, 0);
        assert_eq!(receiver.take_latest().unwrap().jpeg, jpeg);
    }
}
