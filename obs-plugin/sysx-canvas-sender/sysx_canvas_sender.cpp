#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#else
#include <arpa/inet.h>
#include <sys/socket.h>
#include <unistd.h>
#endif

#include <obs-module.h>
#include <obs.h>
#include <media-io/video-io.h>

#include <jpeglib.h>

#include <array>
#include <algorithm>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <optional>
#include <thread>
#include <utility>
#include <vector>

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("sysx-canvas-sender", "en-US")

namespace {

// Compile-time sender configuration. Receiver-side crop remains authoritative.
constexpr char kDestinationIp[] = "192.168.1.100";
constexpr uint16_t kDestinationPort = 4200;
constexpr int kJpegQuality = 80;
constexpr size_t kMaxJpegBytes = 64u * 1024u * 1024u;
constexpr size_t kPayloadBytes = 1170;
constexpr size_t kHeaderBytes = 30;

struct Frame {
  uint32_t width = 0;
  uint32_t height = 0;
  uint32_t stride = 0;
  uint64_t timestamp = 0;
  std::vector<uint8_t> bgra;
};

class Sender {
 public:
  bool start() {
#ifdef _WIN32
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data) != 0) return false;
#endif

    socket_ = ::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
#ifdef _WIN32
    if (socket_ == INVALID_SOCKET) {
      WSACleanup();
      return false;
    }
#else
    if (socket_ < 0) return false;
#endif

    std::memset(&destination_, 0, sizeof(destination_));
    destination_.sin_family = AF_INET;
    destination_.sin_port = htons(kDestinationPort);
    if (::inet_pton(AF_INET, kDestinationIp, &destination_.sin_addr) != 1) {
      close_socket();
      return false;
    }

    worker_ = std::thread([this] { run(); });
    return true;
  }

  void stop() {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      stopping_ = true;
      pending_.reset();
    }
    condition_.notify_one();
    if (worker_.joinable()) worker_.join();
    close_socket();
  }

  void submit(const video_data *video, uint32_t width, uint32_t height,
              uint32_t stride) {
    if (!video || !video->data[0] || width == 0 || height == 0 || stride < width * 4u)
      return;
    Frame next;
    next.width = width;
    next.height = height;
    next.stride = stride;
    next.timestamp = video->timestamp;
    next.bgra.resize(static_cast<size_t>(stride) * height);
    for (uint32_t y = 0; y < height; ++y) {
      std::memcpy(next.bgra.data() + static_cast<size_t>(y) * stride,
                  video->data[0] + static_cast<size_t>(y) * stride, stride);
    }
    {
      std::lock_guard<std::mutex> lock(mutex_);
      if (stopping_) return;
      pending_ = std::move(next); // latest-frame semantics
    }
    condition_.notify_one();
  }

 private:
  void run() {
    for (;;) {
      Frame frame;
      {
        std::unique_lock<std::mutex> lock(mutex_);
        condition_.wait(lock, [this] { return stopping_ || pending_.has_value(); });
        if (stopping_ && !pending_) return;
        frame = std::move(*pending_);
        pending_.reset();
      }
      try {
        send_frame(frame);
      } catch (...) {
        // A bad allocation or encoder failure must never terminate OBS's worker.
      }
    }
  }

  static bool encode_jpeg(const Frame &frame, std::vector<uint8_t> &jpeg) {
    if (frame.width > 65535 || frame.height > 65535 ||
        static_cast<uint64_t>(frame.width) * frame.height > 33554432ull)
      return false;

    std::vector<uint8_t> rgb(static_cast<size_t>(frame.width) * frame.height * 3u);
    for (uint32_t y = 0; y < frame.height; ++y) {
      const uint8_t *src = frame.bgra.data() + static_cast<size_t>(y) * frame.stride;
      uint8_t *dst = rgb.data() + static_cast<size_t>(y) * frame.width * 3u;
      for (uint32_t x = 0; x < frame.width; ++x) {
        dst[x * 3u] = src[x * 4u + 2u];
        dst[x * 3u + 1u] = src[x * 4u + 1u];
        dst[x * 3u + 2u] = src[x * 4u];
      }
    }

    jpeg_compress_struct compressor{};
    jpeg_error_mgr error{};
    compressor.err = jpeg_std_error(&error);
    jpeg_create_compress(&compressor);
    unsigned char *encoded = nullptr;
    unsigned long encoded_size = 0;
    jpeg_mem_dest(&compressor, &encoded, &encoded_size);
    compressor.image_width = frame.width;
    compressor.image_height = frame.height;
    compressor.input_components = 3;
    compressor.in_color_space = JCS_RGB;
    jpeg_set_defaults(&compressor);
    jpeg_set_quality(&compressor, kJpegQuality, TRUE);
    jpeg_start_compress(&compressor, TRUE);
    while (compressor.next_scanline < compressor.image_height) {
      JSAMPROW row = rgb.data() + static_cast<size_t>(compressor.next_scanline) * frame.width * 3u;
      jpeg_write_scanlines(&compressor, &row, 1);
    }
    jpeg_finish_compress(&compressor);
    if (encoded_size <= kMaxJpegBytes) jpeg.assign(encoded, encoded + encoded_size);
    jpeg_destroy_compress(&compressor);
    free(encoded);
    return !jpeg.empty();
  }

  void send_frame(const Frame &frame) {
    std::vector<uint8_t> jpeg;
    if (!encode_jpeg(frame, jpeg) || jpeg.size() > kMaxJpegBytes) return;
    const uint64_t chunks = (jpeg.size() + kPayloadBytes - 1u) / kPayloadBytes;
    if (chunks == 0 || chunks > 65535) return;
    const uint64_t frame_id = next_frame_id_++;
    const uint64_t timestamp = frame.timestamp ? frame.timestamp : monotonic_ns();
    for (uint16_t index = 0; index < chunks; ++index) {
      const size_t offset = static_cast<size_t>(index) * kPayloadBytes;
      const size_t bytes = std::min(kPayloadBytes, jpeg.size() - offset);
      std::array<uint8_t, kHeaderBytes + kPayloadBytes> packet{};
      std::memcpy(packet.data(), "RCVR", 4);
      packet[4] = 1;
      packet[5] = 0;
      put_u64(packet.data() + 6, frame_id);
      put_u16(packet.data() + 14, index);
      put_u16(packet.data() + 16, static_cast<uint16_t>(chunks));
      put_u16(packet.data() + 18, static_cast<uint16_t>(frame.width));
      put_u16(packet.data() + 20, static_cast<uint16_t>(frame.height));
      put_u64(packet.data() + 22, timestamp);
      std::memcpy(packet.data() + kHeaderBytes, jpeg.data() + offset, bytes);
      const int packet_size = static_cast<int>(kHeaderBytes + bytes);
#ifdef _WIN32
      ::sendto(socket_, reinterpret_cast<const char *>(packet.data()), packet_size, 0,
               reinterpret_cast<const sockaddr *>(&destination_), sizeof(destination_));
#else
      ::sendto(socket_, packet.data(), packet_size, 0,
               reinterpret_cast<const sockaddr *>(&destination_), sizeof(destination_));
#endif
    }
  }

  static uint64_t monotonic_ns() {
    return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count());
  }

  static void put_u16(uint8_t *out, uint16_t value) {
    out[0] = static_cast<uint8_t>(value >> 8);
    out[1] = static_cast<uint8_t>(value);
  }
  static void put_u64(uint8_t *out, uint64_t value) {
    for (int i = 7; i >= 0; --i) out[7 - i] = static_cast<uint8_t>(value >> (i * 8));
  }

  void close_socket() {
#ifdef _WIN32
    if (socket_ != INVALID_SOCKET) {
      closesocket(socket_);
      socket_ = INVALID_SOCKET;
      WSACleanup();
    }
#else
    if (socket_ >= 0) {
      close(socket_);
      socket_ = -1;
    }
#endif
  }

  std::mutex mutex_;
  std::condition_variable condition_;
  std::optional<Frame> pending_;
  bool stopping_ = false;
  std::thread worker_;
  uint64_t next_frame_id_ = 1;
#ifdef _WIN32
  SOCKET socket_ = INVALID_SOCKET;
#else
  int socket_ = -1;
#endif
  sockaddr_in destination_{};
};

Sender sender;
uint32_t frame_width = 0;
uint32_t frame_height = 0;

void raw_video_callback(void *, video_data *frame) {
  if (!frame || !frame->data[0]) return;
  try {
    sender.submit(frame, frame_width, frame_height, frame->linesize[0]);
  } catch (...) {
    // Dropping a frame is preferable to propagating an exception into OBS.
  }
}

} // namespace

bool obs_module_load(void) {
  obs_video_info info{};
  obs_get_video_info(&info);
  frame_width = info.base_width;
  frame_height = info.base_height;
  if (frame_width == 0 || frame_height == 0) return false;

  video_scale_info conversion{};
  conversion.format = VIDEO_FORMAT_BGRA;
  conversion.width = 0;  // OBS fills the full main-canvas dimensions.
  conversion.height = 0;
  conversion.range = VIDEO_RANGE_FULL;
  conversion.colorspace = VIDEO_CS_SRGB;
  if (!sender.start()) return false;
  obs_add_raw_video_callback(&conversion, raw_video_callback, nullptr);
  return true;
}

void obs_module_unload(void) {
  obs_remove_raw_video_callback(raw_video_callback, nullptr);
  sender.stop();
}
