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
#include <jpeglib.h>

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
constexpr size_t kPayloadBytes = 1170;
constexpr size_t kHeaderBytes = 30;
constexpr char kControlPrefix[] = "SYSCFG 1 CROP ";
constexpr int kSendBufferBytes = 8 * 1024 * 1024;
// JPEG encoders: a 640 x 640 4:4:4 encode takes about as long as a 240 FPS
// frame interval, so one thread alone dropped frames whenever the game loaded
// the CPU.
constexpr size_t kEncoderThreads = 2;
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
    for (size_t i = 0; i < kEncoderThreads; ++i) workers_.emplace_back([this] { run(); });
    control_worker_ = std::thread([this] { control_loop(); });
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
    if (!data || width == 0 || height == 0 || stride < width * 4u) return;
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
    const size_t row = static_cast<size_t>(width) * 4u;
    next.bgra.resize(row * height);
    for (uint32_t y = 0; y < height; ++y)
      std::memcpy(next.bgra.data() + row * y, data + static_cast<size_t>(stride) * y, row);
    {
      std::lock_guard<std::mutex> lock(mutex_);
      if (stopping_) return;
      // Latest-frame semantics; a replaced frame's buffer is kept for reuse.
      if (pending_ && spare_.size() < kSpareBuffers) spare_.push_back(std::move(pending_->bgra));
      pending_ = std::move(next);
    }
    condition_.notify_one();
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
      if (std::strncmp(message.data(), kControlPrefix, sizeof(kControlPrefix) - 1) != 0) continue;
      const char *digits = message.data() + sizeof(kControlPrefix) - 1;
      char *end = nullptr;
      const auto value = std::strtoul(digits, &end, 10);
      if (end != digits && value >= 1 && value <= 8192) {
        crop_size_.store(static_cast<uint32_t>(value));
        last_control_ns_.store(monotonic_ns());
      }
    }
  }

  void run() {
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
        // A bad allocation or encoder failure must never terminate OBS's worker.
      }
      std::lock_guard<std::mutex> lock(mutex_);
      if (spare_.size() < kSpareBuffers) spare_.push_back(std::move(frame.bgra));
    }
  }

  struct JpegError {
    jpeg_error_mgr manager;
    jmp_buf jump;
  };

  // Returns a malloc'd JPEG buffer (caller frees) or nullptr.
  unsigned char *encode_jpeg(const Frame &frame, unsigned long &size) {
    if (frame.width > 65535 || frame.height > 65535 ||
        static_cast<uint64_t>(frame.width) * frame.height > 33554432ull)
      return nullptr;
#ifndef JCS_EXTENSIONS
    thread_local std::vector<uint8_t> rgb_;
    rgb_.resize(static_cast<size_t>(frame.width) * 3u);
#endif
    jpeg_compress_struct compressor{};
    JpegError error{};
    compressor.err = jpeg_std_error(&error.manager);
    // libjpeg's default handler calls exit(); jump back here instead.
    error.manager.error_exit = [](j_common_ptr info) {
      longjmp(reinterpret_cast<JpegError *>(info->err)->jump, 1);
    };
    unsigned char *encoded = nullptr;
    size = 0;
    if (setjmp(error.jump)) {
      jpeg_destroy_compress(&compressor);
      free(encoded);
      return nullptr;
    }
    jpeg_create_compress(&compressor);
    jpeg_mem_dest(&compressor, &encoded, &size);
    compressor.image_width = frame.width;
    compressor.image_height = frame.height;
#ifdef JCS_EXTENSIONS
    compressor.input_components = 4;
    compressor.in_color_space = JCS_EXT_BGRA;
#else
    compressor.input_components = 3;
    compressor.in_color_space = JCS_RGB;
#endif
    jpeg_set_defaults(&compressor);
    jpeg_set_quality(&compressor, config_.jpeg_quality, TRUE);
    compressor.dct_method = JDCT_IFAST;
    if (config_.chroma_444) {
      compressor.comp_info[0].h_samp_factor = 1;
      compressor.comp_info[0].v_samp_factor = 1;
    }
    jpeg_start_compress(&compressor, TRUE);
    const size_t row = static_cast<size_t>(frame.width) * 4u;
    while (compressor.next_scanline < compressor.image_height) {
      const uint8_t *src = frame.bgra.data() + row * compressor.next_scanline;
#ifdef JCS_EXTENSIONS
      JSAMPROW line = const_cast<JSAMPROW>(src);
#else
      for (uint32_t x = 0; x < frame.width; ++x) {
        rgb_[x * 3u] = src[x * 4u + 2u];
        rgb_[x * 3u + 1u] = src[x * 4u + 1u];
        rgb_[x * 3u + 2u] = src[x * 4u];
      }
      JSAMPROW line = rgb_.data();
#endif
      jpeg_write_scanlines(&compressor, &line, 1);
    }
    jpeg_finish_compress(&compressor);
    jpeg_destroy_compress(&compressor);
    if (size == 0 || size > kMaxJpegBytes) {
      free(encoded);
      return nullptr;
    }
    return encoded;
  }

  void send_frame(const Frame &frame) {
    unsigned long size = 0;
    unsigned char *jpeg = encode_jpeg(frame, size);
    if (!jpeg) return;
    const uint64_t chunks = (size + kPayloadBytes - 1u) / kPayloadBytes;
    if (chunks == 0 || chunks > 65535) {
      free(jpeg);
      return;
    }
    const uint64_t frame_id = frame.id;
    std::array<uint8_t, kHeaderBytes + kPayloadBytes> packet{};
    std::memcpy(packet.data(), "RCVR", 4);
    packet[4] = 1; // version
    packet[5] = 0; // flags
    put_u64(packet.data() + 6, frame_id);
    put_u16(packet.data() + 16, static_cast<uint16_t>(chunks));
    put_u16(packet.data() + 18, static_cast<uint16_t>(frame.width));
    put_u16(packet.data() + 20, static_cast<uint16_t>(frame.height));
    put_u64(packet.data() + 22, frame.timestamp);
    for (uint64_t index = 0; index < chunks; ++index) {
      const size_t offset = static_cast<size_t>(index) * kPayloadBytes;
      const size_t bytes = std::min<size_t>(kPayloadBytes, size - offset);
      put_u16(packet.data() + 14, static_cast<uint16_t>(index));
      std::memcpy(packet.data() + kHeaderBytes, jpeg + offset, bytes);
      ::sendto(socket_, reinterpret_cast<const char *>(packet.data()),
               static_cast<int>(kHeaderBytes + bytes), 0,
               reinterpret_cast<const sockaddr *>(&destination_), sizeof(destination_));
    }
    free(jpeg);
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
    ensure_resources(crop_size);
    if (!crop_texture_) return;

    const uint32_t left = (source_width - crop_size) / 2u;
    const uint32_t top = (source_height - crop_size) / 2u;
    obs_api.copy_texture_region(crop_texture_, 0, 0, main_texture, left, top, crop_size, crop_size);
    last_capture_ns_ = now;
    if (direct_ready_ || setup_direct()) {
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
  bool setup_direct() {
    if (direct_unavailable_ || !obs_api.get_device_obj || !obs_api.texture_get_obj) return false;
    auto *device = static_cast<ID3D11Device *>(obs_api.get_device_obj());
    auto *source = static_cast<ID3D11Texture2D *>(obs_api.texture_get_obj(crop_texture_));
    if (!device || !source) {
      direct_unavailable_ = true;
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
        direct_unavailable_ = true;
        return false;
      }
    }
    device->GetImmediateContext(&context_);
    if (!context_) {
      for (auto *&created : direct_) {
        if (created) created->Release();
        created = nullptr;
      }
      direct_unavailable_ = true;
      return false;
    }
    direct_order_.fill(0);
    direct_ready_ = true;
    return true;
  }

  void render_direct(uint64_t now, uint32_t crop_size) {
    auto *source = static_cast<ID3D11Texture2D *>(obs_api.texture_get_obj(crop_texture_));
    if (!source) return;
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
      }
      const uint64_t done = direct_order_[newest];
      for (auto &order : direct_order_)
        if (order != 0 && order <= done) order = 0;
      return;
    }
  }

  void ensure_resources(uint32_t crop_size) {
    if (crop_size_ == crop_size && crop_texture_) return;
    destroy();
    crop_texture_ = obs_api.texture_create(crop_size, crop_size, GS_BGRA, 1, nullptr, GS_RENDER_TARGET);
    if (!crop_texture_) return;
    for (auto *&stage : stages_) stage = obs_api.stage_create(crop_size, crop_size, GS_BGRA);
    if (!stages_[0] || !stages_[1] || !stages_[2]) {
      destroy();
      return;
    }
    crop_size_ = crop_size;
  }

  gs_texture_t *crop_texture_ = nullptr;
  std::array<gs_stagesurf_t *, kStageCount> stages_{};
  std::array<bool, kStageCount> written_{};
  std::array<uint64_t, kStageCount> captured_ns_{};
  uint32_t crop_size_ = 0;
  size_t write_index_ = 0;
  uint64_t last_capture_ns_ = 0;
  ID3D11DeviceContext *context_ = nullptr;
  std::array<ID3D11Texture2D *, kDirectStages> direct_{};
  std::array<uint64_t, kDirectStages> direct_order_{}; // 0 = free
  std::array<uint64_t, kDirectStages> direct_captured_ns_{};
  uint64_t direct_sequence_ = 0;
  bool direct_ready_ = false;
  bool direct_unavailable_ = false;
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
  if (!obs_api.load()) return false;
  if (!sender.start(Config{})) return false;
  obs_api.add_rendered(main_rendered_callback, nullptr);
  loaded = true;
  return true;
}

void obs_module_unload(void) {
  if (!loaded) return;
  obs_api.remove_rendered(main_rendered_callback, nullptr);
  obs_api.enter_graphics();
  gpu_capture.destroy();
  obs_api.leave_graphics();
  sender.stop();
  loaded = false;
}

#ifdef SYSX_SENDER_SELFTEST
// Headless check of the encode/packetize/control path without OBS:
//   sysx-sender-selftest.exe <port>
// against `receiver-backend udp-listen --host 127.0.0.1 --port <port> --count 3`.
int main(int argc, char **argv) {
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
