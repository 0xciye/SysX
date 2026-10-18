// SysX OBS canvas sender: captures the OBS main texture, crops the
// receiver-requested centered square on the GPU, reads it back without ever
// blocking OBS's render thread, JPEG-encodes it on worker threads and sends it
// to SysX using the RCVR v1 UDP protocol (docs/OBS_RECEIVER_PROTOCOL.md).
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <d3d11.h>

#include <obs-module.h>
#include <graphics/graphics.h>

#include <csetjmp>
#include <cstdio>
// d3d11.h pulls in rpcndr.h, which already defines `boolean` as unsigned
// char, the type libjpeg-turbo's Windows jconfig.h uses ("per Windows
// custom"). Keep jmorecfg.h from declaring it a second time with headers
// that lack that Windows branch.
#ifndef HAVE_BOOLEAN
#define HAVE_BOOLEAN
#endif
#include <jpeglib.h>
#include <jerror.h>

#ifndef JCS_EXTENSIONS
#error "SysX sender requires libjpeg-turbo BGRA input support"
#endif

#include <algorithm>
#include <array>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <mutex>
#include <optional>
#include <string>
#include <string_view>
#include <thread>
#include <utility>
#include <vector>

OBS_DECLARE_MODULE()

MODULE_EXPORT const char *obs_module_name(void) { return "SysX Canvas Sender"; }
MODULE_EXPORT const char *obs_module_description(void) {
  return "Streams the centered OBS canvas crop to a SysX receiver over UDP.";
}

namespace {

constexpr size_t kMaxJpegBytes = 64u * 1024u * 1024u;
constexpr uint64_t kMaxFramePixels = 33554432ull;
constexpr size_t kJpegRowsPerWrite = 8;
constexpr size_t kPayloadBytes = 1170;
constexpr size_t kHeaderBytes = 30;
constexpr uint64_t kMaxSendAgeNs = 100000000ull;
constexpr size_t kMaxRetainedBufferBytes = 16u * 1024u * 1024u;
// Keep square SYSCFG crops within the receiver's 32 Mi-pixel frame budget.
constexpr uint32_t kMaxCropResolution = 5792;
static_assert(static_cast<uint64_t>(kMaxCropResolution) * kMaxCropResolution <=
                  kMaxFramePixels &&
              static_cast<uint64_t>(kMaxCropResolution + 1) *
                      (kMaxCropResolution + 1) >
                  kMaxFramePixels);
constexpr char kControlPrefix[] = "SYSCFG 1 CROP ";
constexpr int kSendBufferBytes = 8 * 1024 * 1024;
// JPEG encoders: a 640 x 640 4:4:4 encode takes about as long as a 240 FPS
// frame interval, so one thread alone dropped frames whenever the game loaded
// the CPU.
constexpr size_t kEncoderThreads = 2;
constexpr uint64_t kResourceRetryNs = 1000000000ull;
// Without a SYSCFG from the receiver for this long (SysX not running or not
// reachable), capture only kIdleFps frames per second: enough for a receiver
// that starts to see the sender and answer, without encoding and sending 240
// frames per second to nobody while the game runs. The receiver repeats
// SYSCFG every second while it receives.
constexpr uint64_t kReceiverQuietNs = 3000000000ull;
constexpr uint32_t kIdleFps = 2;
// Frame buffers kept for reuse, so a 640 x 640 frame (1.6 MB) is not
// allocated on OBS's render thread every frame.
constexpr size_t kSpareBuffers = kEncoderThreads + 1;

// ---- Sender configuration: edit these and rebuild the DLL. ----
// SysX receiver IPv4 address (or hostname) and UDP port.
#ifndef SYSX_RECEIVER_HOST
#define SYSX_RECEIVER_HOST "192.168.88.59"
#endif
constexpr char kReceiverHost[] = SYSX_RECEIVER_HOST;
constexpr uint16_t kReceiverPort = 4200;
// JPEG quality 1-100.
constexpr int kJpegQuality = 80;
// Frames per second cap; 0 sends every OBS render frame.
constexpr uint32_t kMaxFps = 0;
// true = 4:4:4 chroma (keeps small Pixel Seek colors intact), false = 4:2:0.
constexpr bool kChroma444 = true;
// Crop used until the receiver's first SYSCFG control packet arrives.
constexpr uint32_t kDefaultCrop = 640;
// ----------------------------------------------------------------

struct Config {
  std::string host = kReceiverHost;
  uint16_t port = kReceiverPort;
  int jpeg_quality = kJpegQuality;
  uint32_t max_fps = kMaxFps;
  bool chroma_444 = kChroma444;
  uint32_t default_crop = kDefaultCrop;
};

// libobs functions are resolved from the already-loaded obs.dll so the module
// builds against headers plus a minimal import library.
struct ObsApi {
  using AddRendered = void (*)(void (*)(void *), void *);
  using RemoveRendered = void (*)(void (*)(void *), void *);
  using GetMainTexture = gs_texture_t *(*)();
  using EnterGraphics = void (*)();
  using LeaveGraphics = void (*)();
  using TextureSize = uint32_t (*)(const gs_texture_t *);
  using TextureFormat = enum gs_color_format (*)(const gs_texture_t *);
  using TextureCreate = gs_texture_t *(*)(uint32_t, uint32_t, enum gs_color_format,
                                           uint32_t, const uint8_t **, uint32_t);
  using TextureDestroy = void (*)(gs_texture_t *);
  using CopyTextureRegion = void (*)(gs_texture_t *, uint32_t, uint32_t, gs_texture_t *,
                                     uint32_t, uint32_t, uint32_t, uint32_t);
  using StageTexture = void (*)(gs_stagesurf_t *, gs_texture_t *);
  using StageCreate = gs_stagesurf_t *(*)(uint32_t, uint32_t, enum gs_color_format);
  using StageDestroy = void (*)(gs_stagesurf_t *);
  using StageMap = bool (*)(gs_stagesurf_t *, uint8_t **, uint32_t *);
  using StageUnmap = void (*)(gs_stagesurf_t *);
  using GetDeviceObj = void *(*)();
  using TextureGetObj = void *(*)(gs_texture_t *);

  AddRendered add_rendered = nullptr;
  RemoveRendered remove_rendered = nullptr;
  GetMainTexture get_main_texture = nullptr;
  EnterGraphics enter_graphics = nullptr;
  LeaveGraphics leave_graphics = nullptr;
  TextureSize texture_width = nullptr;
  TextureSize texture_height = nullptr;
  TextureFormat texture_format = nullptr;
  TextureCreate texture_create = nullptr;
  TextureDestroy texture_destroy = nullptr;
  CopyTextureRegion copy_texture_region = nullptr;
  StageTexture stage_texture = nullptr;
  StageCreate stage_create = nullptr;
  StageDestroy stage_destroy = nullptr;
  StageMap stage_map = nullptr;
  StageUnmap stage_unmap = nullptr;
  // Optional: the D3D11 device and texture behind libobs objects, for the
  // non-blocking readback. Missing (or another renderer) = libobs staging.
  GetDeviceObj get_device_obj = nullptr;
  TextureGetObj texture_get_obj = nullptr;

  template <typename T>
  static void resolve(HMODULE module, const char *name, T &out) {
    out = reinterpret_cast<T>(GetProcAddress(module, name));
  }

  bool load() {
    HMODULE module = GetModuleHandleA("obs.dll");
    if (!module) return false;
    resolve(module, "obs_add_main_rendered_callback", add_rendered);
    resolve(module, "obs_remove_main_rendered_callback", remove_rendered);
    resolve(module, "obs_get_main_texture", get_main_texture);
    resolve(module, "obs_enter_graphics", enter_graphics);
    resolve(module, "obs_leave_graphics", leave_graphics);
    resolve(module, "gs_texture_get_width", texture_width);
    resolve(module, "gs_texture_get_height", texture_height);
    resolve(module, "gs_texture_get_color_format", texture_format);
    resolve(module, "gs_texture_create", texture_create);
    resolve(module, "gs_texture_destroy", texture_destroy);
    resolve(module, "gs_copy_texture_region", copy_texture_region);
    resolve(module, "gs_stage_texture", stage_texture);
    resolve(module, "gs_stagesurface_create", stage_create);
    resolve(module, "gs_stagesurface_destroy", stage_destroy);
    resolve(module, "gs_stagesurface_map", stage_map);
    resolve(module, "gs_stagesurface_unmap", stage_unmap);
    resolve(module, "gs_get_device_obj", get_device_obj);
    resolve(module, "gs_texture_get_obj", texture_get_obj);
    return add_rendered &&remove_rendered && get_main_texture && enter_graphics &&
           leave_graphics && texture_width && texture_height && texture_format &&
           texture_create && texture_destroy && copy_texture_region && stage_texture &&
           stage_create && stage_destroy && stage_map && stage_unmap;
  }
};

ObsApi obs_api;


uint64_t monotonic_ns() {
  return static_cast<uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(
      std::chrono::steady_clock::now().time_since_epoch()).count());
}

bool is_newer_frame(uint64_t candidate, uint64_t sent) { return candidate > sent; }

bool frame_is_fresh(uint64_t captured_ns, uint64_t now_ns) {
  return captured_ns <= now_ns && now_ns - captured_ns <= kMaxSendAgeNs;
}

bool retry_ready(uint64_t now_ns, uint64_t retry_after_ns) { return now_ns >= retry_after_ns; }

bool parse_crop_resolution(std::string_view digits, uint32_t &resolution) {
  if (digits.empty()) return false;
  uint32_t parsed = 0;
  for (const unsigned char character : digits) {
    if (character < '0' || character > '9') return false;
    const uint32_t digit = character - '0';
    if (parsed > (kMaxCropResolution - digit) / 10) return false;
    parsed = parsed * 10 + digit;
  }
  if (parsed == 0) return false;
  resolution = parsed;
  return true;
}

bool can_send_frame(uint64_t candidate, uint64_t sent, uint64_t captured_ns,
                    uint64_t now_ns) {
  return is_newer_frame(candidate, sent) && frame_is_fresh(captured_ns, now_ns);
}

bool should_release_buffer(size_t capacity, size_t required) {
  return capacity > kMaxRetainedBufferBytes && capacity / 2 > required;
}

struct Frame {
  uint64_t id = 0; // assigned in capture order, so encoders may finish out of order
  uint32_t width = 0;
  uint32_t height = 0;
  uint64_t timestamp = 0;
  std::vector<uint8_t> bgra; // tightly packed, width * 4 bytes per row
};

class Sender {
 public:
  bool start(const Config &config) {
    config_ = config;
    crop_size_.store(config.default_crop);
    WSADATA data{};
    if (WSAStartup(MAKEWORD(2, 2), &data) != 0) return false;
    winsock_ready_ = true;

    addrinfo hints{};
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_DGRAM;
    addrinfo *resolved = nullptr;
    const std::string port = std::to_string(config.port);
    if (getaddrinfo(config.host.c_str(), port.c_str(), &hints, &resolved) != 0 || !resolved) {
      close_socket();
      return false;
    }
    std::memcpy(&destination_, resolved->ai_addr, sizeof(destination_));
    freeaddrinfo(resolved);

    socket_ = ::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    if (socket_ == INVALID_SOCKET) {
      close_socket();
      return false;
    }
    // A 640 x 640 frame is ~100 datagrams; the default 64 KiB buffer drops
    // chunks of nearly every frame at high FPS.
    const int send_buffer = kSendBufferBytes;
    setsockopt(socket_, SOL_SOCKET, SO_SNDBUF, reinterpret_cast<const char *>(&send_buffer),
               sizeof(send_buffer));
    u_long nonblocking = 1;
    if (ioctlsocket(socket_, FIONBIO, &nonblocking) != 0) {
      close_socket();
      return false;
    }
    stopping_ = false;
    try {
      workers_.reserve(kEncoderThreads);
      // The worker returns one buffer per encoder and capture may keep one
      // pending; reserve once so recycling never allocates on a worker thread.
      spare_.reserve(kSpareBuffers);
      for (size_t i = 0; i < kEncoderThreads; ++i) workers_.emplace_back([this] { run(); });
      control_worker_ = std::thread([this] {
        try {
          control_loop();
        } catch (...) {
          // No exception may escape a background thread into std::terminate.
        }
      });
    } catch (...) {
      // Thread creation can fail under resource pressure. Join anything
      // already started before returning through OBS's module-load boundary.
      stop();
      return false;
    }
    return true;
  }

  void stop() {
    {
      std::lock_guard<std::mutex> lock(mutex_);
      stopping_ = true;
      pending_.reset();
    }
    condition_.notify_all();
    for (auto &worker : workers_)
      if (worker.joinable()) worker.join();
    workers_.clear();
    if (control_worker_.joinable()) control_worker_.join();
    close_socket();
  }

  uint32_t crop_size() const { return crop_size_.load(); }
  // A receiver answered (SYSCFG) within kReceiverQuietNs.
  bool receiver_active(uint64_t now) const {
    const uint64_t last = last_control_ns_.load();
    return last != 0 && now - last < kReceiverQuietNs;
  }
  uint32_t max_fps() const { return config_.max_fps; }

  void submit_mapped(const uint8_t *data, uint32_t width, uint32_t height, uint32_t stride,
                     uint64_t timestamp) {
    if (!data || width == 0 || height == 0) return;
    const size_t row = static_cast<size_t>(width) * 4u;
    // Match the encoder's dimension limit before allocating/copying a frame;
    // otherwise oversized crops churn large buffers only to be rejected later.
    if (row > stride || static_cast<uint64_t>(width) * height > kMaxFramePixels) return;
    try {
      Frame next;
      {
        // Reuse an allocation a worker has handed back.
        std::lock_guard<std::mutex> lock(mutex_);
        if (!spare_.empty()) {
          next.bgra.swap(spare_.back());
          spare_.pop_back();
        }
      }
      next.id = next_frame_id_.fetch_add(1);
      next.width = width;
      next.height = height;
      next.timestamp = timestamp;
      const size_t frame_bytes = row * height;
      if (should_release_buffer(next.bgra.capacity(), frame_bytes)) {
        std::vector<uint8_t> replacement;
        next.bgra.swap(replacement);
      }
      next.bgra.resize(frame_bytes);
      if (stride == row) {
        std::memcpy(next.bgra.data(), data, row * height);
      } else {
        for (uint32_t y = 0; y < height; ++y)
          std::memcpy(next.bgra.data() + row * y, data + static_cast<size_t>(stride) * y, row);
      }
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (stopping_) return;
        if (!is_newer_frame(
                next.id, latest_submitted_frame_id_.load(std::memory_order_relaxed))) {
          // Reuse a stale concurrent capture instead of freeing its full-frame buffer.
          if (spare_.size() < kSpareBuffers) spare_.push_back(std::move(next.bgra));
          return;
        }
        // Latest-frame semantics; a replaced frame's buffer is kept for reuse.
        if (pending_ && spare_.size() < kSpareBuffers)
          spare_.push_back(std::move(pending_->bgra));
        latest_submitted_frame_id_.store(next.id, std::memory_order_relaxed);
        pending_ = std::move(next);
      }
      condition_.notify_one();
    } catch (...) {
      // OBS calls this from render/video callbacks. Allocation or lock errors
      // must drop a frame, never unwind across the host's callback boundary.
      return;
    }
  }

 private:
  void control_loop() {
    std::array<char, 128> message{};
    for (;;) {
      {
        std::lock_guard<std::mutex> lock(mutex_);
        if (stopping_) return;
      }
      fd_set readable;
      FD_ZERO(&readable);
      FD_SET(socket_, &readable);
      timeval timeout{0, 100000};
      if (::select(0, &readable, nullptr, nullptr, &timeout) <= 0) continue;
      sockaddr_in from{};
      int from_size = sizeof(from);
      const int received = ::recvfrom(socket_, message.data(), static_cast<int>(message.size() - 1),
                                      0, reinterpret_cast<sockaddr *>(&from), &from_size);
      if (received <= 0) continue;
      // Only the configured receiver may change the crop.
      if (from.sin_addr.s_addr != destination_.sin_addr.s_addr || from.sin_port != destination_.sin_port)
        continue;
      message[static_cast<size_t>(received)] = '\0';
      constexpr size_t prefix_size = sizeof(kControlPrefix) - 1;
      if (static_cast<size_t>(received) <= prefix_size ||
          std::memcmp(message.data(), kControlPrefix, prefix_size) != 0)
        continue;
      const std::string_view digits(message.data() + prefix_size,
                                    static_cast<size_t>(received) - prefix_size);
      uint32_t resolution = 0;
      if (parse_crop_resolution(digits, resolution)) {
        crop_size_.store(resolution);
        last_control_ns_.store(monotonic_ns());
      }
    }
  }

  void run() noexcept {
    try {
      for (;;) {
        Frame frame;
        {
          std::unique_lock<std::mutex> lock(mutex_);
          condition_.wait(lock, [this] { return stopping_ || pending_.has_value(); });
          if (stopping_) return;
          frame = std::move(*pending_);
          pending_.reset();
        }
        try {
          send_frame(frame);
        } catch (...) {
          // A bad allocation or encoder failure drops this frame.
        }
        try {
          std::lock_guard<std::mutex> lock(mutex_);
          if (spare_.size() < kSpareBuffers) spare_.push_back(std::move(frame.bgra));
        } catch (...) {
          // Recycling is optional; never let an allocation/lock error terminate OBS.
        }
      }
    } catch (...) {
      // Even synchronization failures must not escape this std::thread.
    }
  }

  struct JpegError {
    jpeg_error_mgr manager;
    jmp_buf jump;
  };

  struct JpegBufferDestination {
    jpeg_destination_mgr manager{};
    std::vector<JOCTET> *buffer = nullptr;
  };

  struct JpegEncoderState {
    jpeg_compress_struct compressor{};
    JpegError error{};
    std::vector<JOCTET> output;
    bool created = false;
    bool configured = false;

    ~JpegEncoderState() {
      if (created) jpeg_destroy_compress(&compressor);
    }
  };

  static JpegBufferDestination *jpeg_destination(j_compress_ptr compressor) {
    return reinterpret_cast<JpegBufferDestination *>(compressor->dest);
  }

  static void init_jpeg_destination(j_compress_ptr compressor) {
    auto *destination = jpeg_destination(compressor);
    auto &buffer = *destination->buffer;
    const size_t initial_size = std::min(
        std::max(buffer.capacity(), size_t{4096}), kMaxJpegBytes);
    try {
      buffer.resize(initial_size);
    } catch (...) {
      ERREXIT(compressor, JERR_OUT_OF_MEMORY);
    }
    destination->manager.next_output_byte = buffer.data();
    destination->manager.free_in_buffer = buffer.size();
  }

  static boolean grow_jpeg_destination(j_compress_ptr compressor) {
    auto *destination = jpeg_destination(compressor);
    auto &buffer = *destination->buffer;
    const size_t old_size = buffer.size();
    if (old_size >= kMaxJpegBytes) ERREXIT(compressor, JERR_OUT_OF_MEMORY);
    const size_t new_size = old_size + std::min(old_size, kMaxJpegBytes - old_size);
    try {
      buffer.resize(new_size);
    } catch (...) {
      ERREXIT(compressor, JERR_OUT_OF_MEMORY);
    }
    destination->manager.next_output_byte = buffer.data() + old_size;
    destination->manager.free_in_buffer = buffer.size() - old_size;
    return TRUE;
  }

  static void finish_jpeg_destination(j_compress_ptr compressor) {
    auto *destination = jpeg_destination(compressor);
    auto &buffer = *destination->buffer;
    buffer.resize(buffer.size() - destination->manager.free_in_buffer);
  }

  // Reuses each encoder thread's compressor and output buffer; callers must
  // finish sending the result before that worker encodes another frame.
  unsigned char *encode_jpeg(const Frame &frame, unsigned long &size) {
    if (frame.width > 65535 || frame.height > 65535 ||
        static_cast<uint64_t>(frame.width) * frame.height > kMaxFramePixels)
      return nullptr;
    thread_local JpegEncoderState encoder;
    auto &compressor = encoder.compressor;
    size = 0;
    const auto estimated_output = static_cast<size_t>(std::min<uint64_t>(
        static_cast<uint64_t>(frame.width) * frame.height * 3u, kMaxJpegBytes));
    if (should_release_buffer(encoder.output.capacity(), estimated_output)) {
      std::vector<JOCTET> replacement;
      encoder.output.swap(replacement);
    }
    if (setjmp(encoder.error.jump)) {
      // The destination manager lives on this stack frame; detach it before
      // destroying the compressor or attempting another frame.
      compressor.dest = nullptr;
      jpeg_destroy_compress(&compressor);
      encoder.created = false;
      encoder.configured = false;
      encoder.output.clear();
      return nullptr;
    }
    if (!encoder.created) {
      compressor.err = jpeg_std_error(&encoder.error.manager);
      // jpeg_std_error installs an exit()-based handler; replace it after
      // initialization so encode failures return to this worker instead.
      compressor.err->error_exit = [](j_common_ptr info) {
        longjmp(reinterpret_cast<JpegError *>(info->err)->jump, 1);
      };
      // Let the longjmp cleanup destroy any partial libjpeg state if
      // compressor creation itself fails under memory pressure.
      encoder.created = true;
      jpeg_create_compress(&compressor);
    }
    const bool initialize = !encoder.configured;
    JpegBufferDestination destination{};
    destination.buffer = &encoder.output;
    destination.manager.init_destination = init_jpeg_destination;
    destination.manager.empty_output_buffer = grow_jpeg_destination;
    destination.manager.term_destination = finish_jpeg_destination;
    compressor.dest = &destination.manager;
    compressor.image_width = frame.width;
    compressor.image_height = frame.height;
    if (initialize) {
      compressor.input_components = 4;
      compressor.in_color_space = JCS_EXT_BGRA;
      jpeg_set_defaults(&compressor);
      jpeg_set_quality(&compressor, config_.jpeg_quality, TRUE);
      compressor.dct_method = JDCT_IFAST;
      if (config_.chroma_444) {
        compressor.comp_info[0].h_samp_factor = 1;
        compressor.comp_info[0].v_samp_factor = 1;
      }
      encoder.configured = true;
    }
    jpeg_start_compress(&compressor, TRUE);
    const size_t row = static_cast<size_t>(frame.width) * 4u;
    while (compressor.next_scanline < compressor.image_height) {
      std::array<JSAMPROW, kJpegRowsPerWrite> lines{};
      const size_t rows = std::min<size_t>(
          kJpegRowsPerWrite, compressor.image_height - compressor.next_scanline);
      for (size_t i = 0; i < rows; ++i) {
        const uint8_t *src = frame.bgra.data() +
                             row * (static_cast<size_t>(compressor.next_scanline) + i);
        lines[i] = const_cast<JSAMPROW>(src);
      }
      if (jpeg_write_scanlines(&compressor, lines.data(), static_cast<JDIMENSION>(rows)) == 0) {
        compressor.dest = nullptr;
        jpeg_abort_compress(&compressor);
        encoder.output.clear();
        return nullptr;
      }
    }
    jpeg_finish_compress(&compressor);
    compressor.dest = nullptr;
    if (encoder.output.empty() || encoder.output.size() > kMaxJpegBytes) {
      encoder.output.clear();
      return nullptr;
    }
    size = static_cast<unsigned long>(encoder.output.size());
    return encoder.output.data();
  }

  void send_frame(const Frame &frame) {
    if (!can_send_frame(frame.id, latest_sent_frame_id_.load(std::memory_order_relaxed),
                        frame.timestamp, monotonic_ns()))
      return;
    unsigned long size = 0;
    unsigned char *jpeg = encode_jpeg(frame, size);
    if (!jpeg) return;
    const uint64_t chunks = (size + kPayloadBytes - 1u) / kPayloadBytes;
    if (chunks == 0 || chunks > 65535) return;
    std::lock_guard<std::mutex> send_lock(send_mutex_);
    // Keep fresh in-flight encodes eligible; requiring the newest capture here
    // starves output whenever JPEG encoding takes longer than one frame interval.
    if (!can_send_frame(frame.id, latest_sent_frame_id_.load(std::memory_order_relaxed),
                        frame.timestamp, monotonic_ns()))
      return;
    // The send mutex and monotonic frame IDs preserve output order.
    latest_sent_frame_id_.store(frame.id, std::memory_order_relaxed);
    const uint64_t frame_id = frame.id;
    std::array<uint8_t, kHeaderBytes> header{};
    std::memcpy(header.data(), "RCVR", 4);
    header[4] = 1; // version
    header[5] = 0; // flags
    put_u64(header.data() + 6, frame_id);
    put_u16(header.data() + 16, static_cast<uint16_t>(chunks));
    put_u16(header.data() + 18, static_cast<uint16_t>(frame.width));
    put_u16(header.data() + 20, static_cast<uint16_t>(frame.height));
    put_u64(header.data() + 22, frame.timestamp);
    WSABUF buffers[2]{};
    buffers[0].buf = reinterpret_cast<char *>(header.data());
    buffers[0].len = static_cast<ULONG>(header.size());
    for (uint64_t index = 0; index < chunks; ++index) {
      // Large JPEGs can take longer than the useful-frame window to send;
      // abandon their tail so the send lock is available to fresher frames.
      if ((index & 63u) == 0 && !frame_is_fresh(frame.timestamp, monotonic_ns())) break;
      const size_t offset = static_cast<size_t>(index) * kPayloadBytes;
      const size_t bytes = std::min<size_t>(kPayloadBytes, size - offset);
      put_u16(header.data() + 14, static_cast<uint16_t>(index));
      buffers[1].buf = const_cast<char *>(reinterpret_cast<const char *>(jpeg + offset));
      buffers[1].len = static_cast<ULONG>(bytes);
      DWORD sent = 0;
      const int result = WSASendTo(socket_, buffers, 2, &sent, 0,
                                   reinterpret_cast<const sockaddr *>(&destination_),
                                   sizeof(destination_), nullptr, nullptr);
      // The socket is nonblocking. A full send buffer means this frame is
      // incomplete, so skip its remaining chunks instead of burning syscalls.
      if (result == SOCKET_ERROR || sent != kHeaderBytes + bytes) break;
    }
  }

  static void put_u16(uint8_t *out, uint16_t value) {
    out[0] = static_cast<uint8_t>(value >> 8);
    out[1] = static_cast<uint8_t>(value);
  }
  static void put_u64(uint8_t *out, uint64_t value) {
    for (int i = 7; i >= 0; --i) out[7 - i] = static_cast<uint8_t>(value >> (i * 8));
  }

  void close_socket() {
    if (socket_ != INVALID_SOCKET) {
      closesocket(socket_);
      socket_ = INVALID_SOCKET;
    }
    if (winsock_ready_) {
      WSACleanup();
      winsock_ready_ = false;
    }
  }

  Config config_;
  std::mutex mutex_;
  std::mutex send_mutex_;
  std::condition_variable condition_;
  std::optional<Frame> pending_;
  std::vector<std::vector<uint8_t>> spare_;
  bool stopping_ = false;
  bool winsock_ready_ = false;
  std::vector<std::thread> workers_;
  std::thread control_worker_;
  std::atomic<uint32_t> crop_size_{640};
  std::atomic<uint64_t> next_frame_id_{1};
  std::atomic<uint64_t> last_control_ns_{0};
  std::atomic<uint64_t> latest_submitted_frame_id_{0};
  std::atomic<uint64_t> latest_sent_frame_id_{0};
  SOCKET socket_ = INVALID_SOCKET;
  sockaddr_in destination_{};
};

Sender sender;

// GPU path: copy the centered crop of the main texture and read it back
// without stalling OBS. libobs' gs_stagesurface_map waits for the GPU; with a
// game loading the GPU, a copy queued two frames earlier was often not done,
// so OBS's render thread blocked in the map and OBS dropped frames. On D3D11
// the crop is copied into a ring of our own staging textures and mapped with
// D3D11_MAP_FLAG_DO_NOT_WAIT: the newest finished copy is sent, unfinished
// ones are tried again next frame, and when all are still in flight the
// oldest is overwritten (that frame is skipped, OBS never waits). Other
// renderers keep the libobs three-surface ring. Only the crop is handed to the
// sender's worker threads.
class GpuCapture {
 public:
  void render() {
    gs_texture_t *main_texture = obs_api.get_main_texture();
    if (!main_texture) return;
    // HDR canvases render to RGBA16F; copy_texture_region needs matching
    // formats, so they are silently not sent.
    if (obs_api.texture_format(main_texture) != GS_BGRA) return;
    const uint64_t now = monotonic_ns();
    uint32_t fps = sender.max_fps();
    if (!sender.receiver_active(now)) fps = fps ? std::min(fps, kIdleFps) : kIdleFps;
    if (fps && now - last_capture_ns_ < 1000000000ull / fps) return;

    const uint32_t source_width = obs_api.texture_width(main_texture);
    const uint32_t source_height = obs_api.texture_height(main_texture);
    const uint32_t crop_size = std::min({sender.crop_size(), source_width, source_height});
    if (crop_size == 0) return;
    if (static_cast<uint64_t>(crop_size) * crop_size > kMaxFramePixels) return;
    ensure_resources(crop_size);
    if (!crop_texture_) return;

    const uint32_t left = (source_width - crop_size) / 2u;
    const uint32_t top = (source_height - crop_size) / 2u;
    obs_api.copy_texture_region(crop_texture_, 0, 0, main_texture, left, top, crop_size, crop_size);
    last_capture_ns_ = now;
    if (direct_ready_ || setup_direct(now)) {
      render_direct(now, crop_size);
      return;
    }
    obs_api.stage_texture(stages_[write_index_], crop_texture_);
    written_[write_index_] = true;
    captured_ns_[write_index_] = now;

    const size_t read_index = (write_index_ + 1) % kStageCount;
    if (written_[read_index]) {
      uint8_t *mapped = nullptr;
      uint32_t linesize = 0;
      if (obs_api.stage_map(stages_[read_index], &mapped, &linesize)) {
        try {
          sender.submit_mapped(mapped, crop_size, crop_size, linesize, captured_ns_[read_index]);
        } catch (...) {
          // A dropped frame must never propagate through OBS's render callback.
        }
        obs_api.stage_unmap(stages_[read_index]);
      }
      written_[read_index] = false;
    }
    write_index_ = read_index;
  }

  void destroy() {
    for (auto *&texture : direct_) {
      if (texture) texture->Release();
      texture = nullptr;
    }
    direct_order_.fill(0);
    if (context_) context_->Release();
    context_ = nullptr;
    direct_ready_ = false;
    direct_unavailable_ = false;
    direct_retry_after_ns_ = 0;
    for (auto *&stage : stages_) {
      if (stage) obs_api.stage_destroy(stage);
      stage = nullptr;
    }
    if (crop_texture_) obs_api.texture_destroy(crop_texture_);
    crop_texture_ = nullptr;
    written_.fill(false);
    crop_size_ = 0;
    write_index_ = 0;
  }

 private:
  static constexpr size_t kStageCount = 3;
  static constexpr size_t kDirectStages = 4;

  // Own D3D11 staging ring for the crop texture; false keeps libobs staging.
  bool setup_direct(uint64_t now) {
    if (!obs_api.get_device_obj || !obs_api.texture_get_obj ||
        (direct_unavailable_ && !retry_ready(now, direct_retry_after_ns_)))
      return false;
    direct_unavailable_ = false;
    auto *device = static_cast<ID3D11Device *>(obs_api.get_device_obj());
    auto *source = static_cast<ID3D11Texture2D *>(obs_api.texture_get_obj(crop_texture_));
    if (!device || !source) {
      defer_direct_retry(now);
      return false;
    }
    D3D11_TEXTURE2D_DESC desc{};
    source->GetDesc(&desc);
    desc.MipLevels = 1;
    desc.ArraySize = 1;
    desc.SampleDesc.Count = 1;
    desc.SampleDesc.Quality = 0;
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
    desc.MiscFlags = 0;
    for (auto *&texture : direct_) {
      if (FAILED(device->CreateTexture2D(&desc, nullptr, &texture))) {
        texture = nullptr;
        for (auto *&created : direct_) {
          if (created) created->Release();
          created = nullptr;
        }
        defer_direct_retry(now);
        return false;
      }
    }
    device->GetImmediateContext(&context_);
    if (!context_) {
      for (auto *&created : direct_) {
        if (created) created->Release();
        created = nullptr;
      }
      defer_direct_retry(now);
      return false;
    }
    direct_order_.fill(0);
    direct_retry_after_ns_ = 0;
    direct_ready_ = true;
    return true;
  }

  void render_direct(uint64_t now, uint32_t crop_size) {
    auto *source = static_cast<ID3D11Texture2D *>(obs_api.texture_get_obj(crop_texture_));
    if (!source) {
      disable_direct();
      return;
    }
    // A free slot, else the oldest copy still in flight (its frame is skipped).
    size_t slot = 0;
    for (size_t i = 0; i < kDirectStages; ++i) {
      if (direct_order_[i] == 0) {
        slot = i;
        break;
      }
      if (direct_order_[i] < direct_order_[slot]) slot = i;
    }
    context_->CopyResource(direct_[slot], source);
    direct_order_[slot] = ++direct_sequence_;
    direct_captured_ns_[slot] = now;

    // The newest finished copy. The GPU runs copies in order, so once one is
    // done every older one is too and can be dropped.
    uint64_t below = UINT64_MAX;
    for (;;) {
      size_t newest = kDirectStages;
      for (size_t i = 0; i < kDirectStages; ++i)
        if (direct_order_[i] != 0 && direct_order_[i] < below &&
            (newest == kDirectStages || direct_order_[i] > direct_order_[newest]))
          newest = i;
      if (newest == kDirectStages) return;
      D3D11_MAPPED_SUBRESOURCE mapped{};
      const HRESULT result =
          context_->Map(direct_[newest], 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT, &mapped);
      if (result == DXGI_ERROR_WAS_STILL_DRAWING) {
        below = direct_order_[newest];
        continue;
      }
      if (SUCCEEDED(result)) {
        try {
          sender.submit_mapped(static_cast<const uint8_t *>(mapped.pData), crop_size, crop_size,
                               mapped.RowPitch, direct_captured_ns_[newest]);
        } catch (...) {
          // A dropped frame must never propagate through OBS's render callback.
        }
        context_->Unmap(direct_[newest], 0);
      } else {
        // Device removal or another staging failure should not leave the
        // sender permanently on a failed D3D11 readback path. The next frame
        // falls back to the existing libobs staging implementation.
        disable_direct();
        return;
      }
      const uint64_t done = direct_order_[newest];
      for (auto &order : direct_order_)
        if (order != 0 && order <= done) order = 0;
      return;
    }
  }

  void disable_direct() {
    for (auto *&texture : direct_) {
      if (texture) texture->Release();
      texture = nullptr;
    }
    if (context_) context_->Release();
    context_ = nullptr;
    direct_order_.fill(0);
    direct_ready_ = false;
    defer_direct_retry(monotonic_ns());
  }

  void defer_direct_retry(uint64_t now) {
    direct_unavailable_ = true;
    direct_retry_after_ns_ = now + kResourceRetryNs;
  }

  void ensure_resources(uint32_t crop_size) {
    if (crop_size_ == crop_size && crop_texture_) return;
    const uint64_t now = monotonic_ns();
    if (last_resource_attempt_crop_ == crop_size && now < resource_retry_after_ns_) return;
    destroy();
    last_resource_attempt_crop_ = crop_size;
    crop_texture_ = obs_api.texture_create(crop_size, crop_size, GS_BGRA, 1, nullptr, GS_RENDER_TARGET);
    if (!crop_texture_) {
      resource_retry_after_ns_ = now + kResourceRetryNs;
      return;
    }
    for (auto *&stage : stages_) stage = obs_api.stage_create(crop_size, crop_size, GS_BGRA);
    if (!stages_[0] || !stages_[1] || !stages_[2]) {
      destroy();
      resource_retry_after_ns_ = now + kResourceRetryNs;
      return;
    }
    crop_size_ = crop_size;
    resource_retry_after_ns_ = 0;
  }

  gs_texture_t *crop_texture_ = nullptr;
  std::array<gs_stagesurf_t *, kStageCount> stages_{};
  std::array<bool, kStageCount> written_{};
  std::array<uint64_t, kStageCount> captured_ns_{};
  uint32_t crop_size_ = 0;
  uint32_t last_resource_attempt_crop_ = 0;
  uint64_t resource_retry_after_ns_ = 0;
  size_t write_index_ = 0;
  uint64_t last_capture_ns_ = 0;
  ID3D11DeviceContext *context_ = nullptr;
  std::array<ID3D11Texture2D *, kDirectStages> direct_{};
  std::array<uint64_t, kDirectStages> direct_order_{}; // 0 = free
  std::array<uint64_t, kDirectStages> direct_captured_ns_{};
  uint64_t direct_sequence_ = 0;
  bool direct_ready_ = false;
  bool direct_unavailable_ = false;
  uint64_t direct_retry_after_ns_ = 0;
};

GpuCapture gpu_capture;

void main_rendered_callback(void *) {
  try {
    gpu_capture.render();
  } catch (...) {
    // Never let an allocation/graphics failure escape into OBS.
  }
}

bool loaded = false;

} // namespace

bool obs_module_load(void) {
  try {
    if (!obs_api.load()) return false;
    if (!sender.start(Config{})) return false;
    obs_api.add_rendered(main_rendered_callback, nullptr);
    loaded = true;
    return true;
  } catch (...) {
    try {
      sender.stop();
    } catch (...) {
      // Exceptions must not cross OBS's C module-loading boundary.
    }
    return false;
  }
}

void obs_module_unload(void) {
  if (!loaded) return;
  loaded = false;
  try {
    obs_api.remove_rendered(main_rendered_callback, nullptr);
    obs_api.enter_graphics();
    gpu_capture.destroy();
    obs_api.leave_graphics();
  } catch (...) {
    // Continue shutting down sender workers even if graphics teardown fails.
  }
  try {
    sender.stop();
  } catch (...) {
    // Exceptions must not cross OBS's C module-unloading boundary.
  }
}

#ifdef SYSX_SENDER_SELFTEST
// Headless check of the encode/packetize/control path without OBS:
//   sysx-sender-selftest.exe <port>
// against `receiver-backend udp-listen --host 127.0.0.1 --port <port> --count 3`.
int main(int argc, char **argv) {
  uint32_t crop = 0;
  if (!parse_crop_resolution("1", crop) || crop != 1 ||
      !parse_crop_resolution("5792", crop) || crop != kMaxCropResolution ||
      parse_crop_resolution("0", crop) || parse_crop_resolution("5793", crop) ||
      parse_crop_resolution("256junk", crop) || parse_crop_resolution(" 256", crop) ||
      parse_crop_resolution("4294967295", crop) ||
      parse_crop_resolution(std::string_view("256\0junk", 8), crop))
    return 5;
  if (!is_newer_frame(2, 1) || is_newer_frame(1, 2) || is_newer_frame(1, 1)) return 3;
  if (!frame_is_fresh(90, 100) || frame_is_fresh(0, kMaxSendAgeNs + 1) ||
      frame_is_fresh(101, 100) || !can_send_frame(2, 1, 90, 100) ||
      can_send_frame(1, 1, 90, 100) ||
      can_send_frame(3, 1, 0, kMaxSendAgeNs + 1) ||
      can_send_frame(3, 1, 101, 100) || retry_ready(99, 100) || !retry_ready(100, 100))
    return 4;
  if (!should_release_buffer(128u * 1024u * 1024u, 2u * 1024u * 1024u) ||
      should_release_buffer(8u * 1024u * 1024u, 2u * 1024u * 1024u))
    return 6;
  Config config;
  config.host = "127.0.0.1"; // Self-test never sends to the production LAN host.
  config.port = static_cast<uint16_t>(argc > 1 ? std::atoi(argv[1]) : 4299);
  config.default_crop = 320; // the receiver must replace this with its 640
  if (!sender.start(config)) return 1;
  struct Size { uint32_t width, height, padding; };
  for (const Size size : {Size{320, 320, 0}, Size{200, 120, 64}, Size{1, 1, 0}}) {
    const uint32_t stride = size.width * 4u + size.padding;
    std::vector<uint8_t> image(static_cast<size_t>(stride) * size.height);
    for (uint32_t y = 0; y < size.height; ++y)
      for (uint32_t x = 0; x < size.width; ++x) {
        uint8_t *pixel = image.data() + static_cast<size_t>(stride) * y + x * 4u;
        pixel[0] = static_cast<uint8_t>(x); // B
        pixel[1] = static_cast<uint8_t>(y); // G
        pixel[2] = 255;                     // R
        pixel[3] = 255;
      }
    sender.submit_mapped(image.data(), size.width, size.height, stride, monotonic_ns());
    std::this_thread::sleep_for(std::chrono::milliseconds(150));
  }
  for (int i = 0; i < 20 && sender.crop_size() != 640; ++i)
    std::this_thread::sleep_for(std::chrono::milliseconds(50));
  const uint32_t crop = sender.crop_size();
  // Optional: `<port> <seconds>` keeps streaming a 640x640 dark frame with a
  // moving red square (a Pixel Seek "red" target) at ~60 FPS for UI checks.
  const int seconds = argc > 2 ? std::atoi(argv[2]) : 0;
  const uint64_t end = monotonic_ns() + static_cast<uint64_t>(seconds) * 1000000000ull;
  std::vector<uint8_t> frame(640u * 640u * 4u);
  for (uint32_t tick = 0; monotonic_ns() < end; ++tick) {
    const uint32_t left = 360 + (tick % 120) / 2, top = 230;
    for (uint32_t y = 0; y < 640; ++y)
      for (uint32_t x = 0; x < 640; ++x) {
        uint8_t *pixel = frame.data() + (static_cast<size_t>(y) * 640u + x) * 4u;
        const bool target = x >= left && x < left + 40 && y >= top && y < top + 40;
        pixel[0] = target ? 0 : static_cast<uint8_t>(18 + y / 16);
        pixel[1] = target ? 0 : static_cast<uint8_t>(22 + x / 20);
        pixel[2] = target ? 255 : 16;
        pixel[3] = 255;
      }
    sender.submit_mapped(frame.data(), 640, 640, 640u * 4u, monotonic_ns());
    std::this_thread::sleep_for(std::chrono::milliseconds(16));
  }
  sender.stop();
  std::printf("{\"event\":\"selftest\",\"receiver_crop\":%u}\n", crop);
  return crop == 640 ? 0 : 2;
}
#endif
