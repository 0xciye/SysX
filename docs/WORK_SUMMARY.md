# Tổng hợp công việc — 2026-10-15

> **Trạng thái sử dụng:** Hiện chỉ **Pixel Seek dùng được**. **Object Detection, ONNX Runtime, OpenVINO và TensorRT chưa dùng được**; source/dependency được giữ cho phát triển. Thiết bị được hỗ trợ chỉ còn **Medius qua Rust SDK**; đã bỏ Ferrum và MAKCU.

## Cập nhật — 2026-10-17

- Thêm Motion **Always on**: khi bật, chuyển động theo target hợp lệ không cần giữ nút trigger; khi tắt, giữ nguyên cơ chế trigger như trước. Bật Always on sẽ vô hiệu hóa Auto trong UI và ở runtime; các thiết lập Auto vẫn được lưu để dùng lại khi Always on tắt. Target vẫn phải còn mới (≤100 ms) và Motion phải được bật.
- Khôi phục điều kiện sẵn sàng Medius đầy đủ: `link_up`, `mouse_attached`, và `clone_configured` đều phải đúng trước khi nhận input hoặc gửi lệnh. Quá trình dò vẫn độc lập với clone enumeration, thử các cổng CH343 phù hợp và ghi nguyên nhân lỗi vào log.
- Đã build release mới; 49 bài test native Rust đạt. Chưa xác minh trên thiết bị Medius/chuột thật.

## Cập nhật — 2026-10-18

- Chuyển đường cấu hình desktop sang cửa sổ Slint trong app: không tự mở browser, không khởi chạy config HTTP server, không embed/extract bundle WASM; desktop build không cần Trunk.
- Native Settings chỉ hiện “Changes applied successfully.” sau ACK Apply và tự ẩn sau ba giây; nếu lỗi/thông báo khác xuất hiện sau đó, timer không xóa nhầm nội dung mới. Chưa build/test.
- Gỡ endpoint/config page Leptos-WASM và các lệnh build Trunk; `rust-ui` hiện chỉ giữ helper Rust dùng chung cho host. WebSocket IPC chỉ nhận role `host` hoặc `monitor`.
- Thêm UI native để chọn profile theo vòng, tạo/clone, đổi tên và xóa profile; chặn thay profile khi settings hiện tại còn thay đổi chưa Apply. Cửa sổ Settings đang mở giữ backend sống khi monitor bị đóng.
- Pixel Seek giữ vùng target lock 100 ms sau một frame miss thay vì nhảy ngay sang pixel màu khác; Auto không nhận pixel thay thế trong khoảng chờ đó.
- Dự đoán vị trí target lock giờ theo tốc độ đo được trong tối đa 100 ms thay vì cắt cứng mọi dịch chuyển ở 64 px; giảm miss/đổi target khi receiver bỏ frame cũ mà vẫn giữ trần tốc độ.
- Sửa Pixel Seek dùng đủ displacement dự đoán giữa frame thay vì kẹp nhầm ở ±64 px; giới hạn riêng prediction error và không nhận blob mép ảnh khi target dự đoán đã ra ngoài frame. Thêm regression source cho hai edge case; chưa chạy test theo yêu cầu.
- Tách pixel target đã chọn khỏi tâm bbox: Pixel Seek dùng pixel lần trước làm tie-break anchor để không lắc giữa các pixel hợp lệ khi hình blob đổi; tâm bbox vẫn là nguồn đo vận tốc.
- Chặn frame BGRA sai kích thước/stride và bắt lỗi allocation/lock trong OBS callback để bỏ frame thay vì unwind ra ngoài host.
- Center Crossing dùng pixel màu được Pixel Seek chọn: chỉ kích hoạt khi pixel hợp lệ đi qua tâm; không còn suy luận kích thước crosshair hay dò pixel ở hai bên tâm. Crossing được giữ 30 ms để host kịp nhận.
- Bỏ bbox khỏi API Center Crossing: gate chỉ nhận tọa độ pixel đã được Pixel Seek chọn; quan hệ blob do Pixel Seek quản lý. Chưa chạy test theo yêu cầu chỉ sửa source.
- Center Crossing không còn chặn crossing chỉ vì pixel dịch hơn 20 px giữa hai frame; gate reset khi lock Pixel Seek hết hạn, tránh nối crossing qua hai track khác nhau. Có regression source cho frame gap và mất track; chưa chạy test.
- Decode pool giới hạn tổng số pixel đang giải mã ở 8 MP; frame lớn hơn được giải mã độc quyền để tránh nhiều buffer RGB cực lớn cùng lúc. Nếu callback giao frame panic làm poison mutex thứ tự, worker kế tiếp khôi phục khóa và tiếp tục.
- Decode worker bỏ qua frame đã lỗi thời trước khi giải mã và kiểm tra lại sau khi chờ ngân sách pixel; giảm CPU dành cho JPEG cũ khi worker bị nghẽn. Có regression test source; chưa chạy theo yêu cầu.
- Decode pool giữ latest-only cho frame đang chờ, nhưng frame đã bắt đầu decode vẫn được phát nếu chưa bị kết quả mới hơn vượt qua và tuổi dưới 100 ms; tránh bỏ đói pipeline khi decoder chậm hơn FPS đầu vào. Frame quá cũ vẫn bị loại. Có regression source cho thứ tự delivery và giới hạn tuổi; chưa chạy.
- Hàng đợi JPEG khôi phục mutex đã poison thay vì làm worker thoát hoặc âm thầm bỏ frame; có regression test source cho lock recovery, chưa chạy.
- Inbox Preview native cũng khôi phục mutex/condvar bị poison thay vì bỏ frame hoặc kết thúc decoder thread; có regression test source, chưa chạy.
- DrawGate Preview khôi phục khóa/condvar bị poison để tín hiệu redraw không bị âm thầm mất; có regression test source, chưa chạy.
- Host native khôi phục khóa Auto trigger, trạng thái nút chuột và capture bridge khi poison, tránh default/missing giả hoặc bỏ qua command state; regression source hiện có cho device-state lock recovery, chưa chạy.
- Slint Preview khôi phục frame RGBA sau mutex poison thay vì âm thầm bỏ upload của frame đó; giữ nguyên transfer một lần sang image, chưa chạy test.
- Pipeline xử lý cũng khôi phục state mutex bị poison ở submit/configure/worker/read/close, tránh một panic trước đó làm chết luồng xử lý và kẹt receiver; có regression test source, chưa chạy.
- Thời hạn Pixel Seek/Object Detection target lock 100 ms giờ tính từ lúc frame được nhận, không phải lúc xử lý hoàn tất; frame tồn đọng không còn gia hạn lock/Auto freshness. Có regression test source, chưa chạy.
- Sửa tie-break pixel theo X: scan không dừng ngay khi chạm anchor nếu hit tốt nhất bên trái còn xa hơn; tiếp tục tới khi không thể có hit gần hơn, giữ ưu tiên bên trái khi khoảng cách hòa. Có regression test source, chưa chạy.
- Native Preview bỏ lượt đọc JPEG header lặp: `decode_preview_into` vẫn tự xác minh dimensions metadata và giới hạn trước khi giải mã vào buffer.
- Native Preview WebSocket dùng blocking read với timeout 50 ms thay poll nonblocking mỗi 2 ms; frame tới vẫn đánh thức socket ngay, còn shutdown được quan sát theo timeout. Chưa build/test theo yêu cầu.
- Native Preview decoder bắt panic từ callback đánh thức UI và ghi diagnostics thay vì để unwind kết thúc decoder worker; dùng chung panic-recovery helper hiện có.
- Native Preview decoder bọc toàn bộ lượt xử lý worker trong panic recovery; panic ngoài JPEG/callback được ghi vào diagnostics, cập nhật lỗi Preview và tiếp tục sau 50 ms thay vì làm chết thread.
- Release profiles của `src-tauri` và `rust-backend` đặt `panic = "unwind"`, nên các lớp `catch_unwind` của host/receiver vẫn hoạt động ở release; cấu hình `rust-ui` riêng không thay profile của binary host khi dùng làm dependency.
- UDP receiver có supervisor ở biên thread: panic ngoài `read_encoded` được ghi kèm thông tin panic, nguồn được reset và thử bind lại sau backoff; source mutex bị poison được khôi phục/clear. Có regression source cho lock recovery; chưa chạy.
- OBS sender gán lại libjpeg `error_exit` sau `jpeg_std_error`, tránh handler mặc định gọi `exit()` khi encode gặp lỗi. Chỉ sửa source; DLL chưa rebuild nên binary hiện tại chưa có sửa này.
- Sửa comment runtime còn sót từ Web Config: `role=host` không phải config client/server; `monitor_clients` chỉ đếm native monitor và device status được gửi tới monitor native.
- Thêm regression tích hợp Auto Center với Color Filter: hai pixel hợp lệ ở hai phía nhưng không có selected-pixel crossing tâm thì `auto_target_valid` vẫn false; chưa chạy test.
- Đổi nhãn toggle native từ “Disable deadzone” thành “No deadzone”, giữ nguyên key `motionNoDeadzone` và xử lý cài đặt.
- Thêm Pixel Seek regression source: một pixel Yellow hợp lệ ngay gần rìa vòng FOV vẫn tạo target 1×1; không đổi HSV thresholds, chưa chạy test.
- Tối ưu color-filter trên preview downscale: thay phép chia tọa độ từng pixel bằng quotient/remainder accumulator, giữ nguyên lấy mẫu tâm pixel; thêm regression source cho tỷ lệ không nguyên.
- Tối ưu phép tính khoảng cách trong vùng tìm Pixel Seek lock: dùng bình phương nhân trực tiếp và dùng lại khoảng cách theo hàng, giữ nguyên FOV/anchor/tie-break; chưa chạy test theo yêu cầu.
- Giảm nhánh/giá trị trung gian trong hot loop color-filter preview downscale: kiểm tra crop theo hàng và đọc bit mask trực tiếp mỗi pixel, giữ nguyên phép ánh xạ nearest-center và alpha; chưa chạy test theo yêu cầu.
- Sửa pipeline shutdown giữ lại `JoinHandle` khi chờ quá timeout; lần gọi `close()` tiếp theo không báo thành công giả khi worker vẫn chạy. Thêm regression source, chưa chạy test.
- UDP reassembly tính cả JPEG sẵn sàng và buffer JPEG tạm trong lúc nối chunk vào ngân sách bộ nhớ 128 MiB; frame mới vượt peak budget bị loại trước khi cấp phát thêm buffer ghép.
- OBS sender giữ nguyên encoder JPEG theo worker, nhưng chỉ thiết lập defaults/quality/chroma một lần thay vì mỗi frame; chưa build lại DLL nên tối ưu source này chưa có trong binary hiện tại.
- OBS sender giãn retry tạo GPU crop/staging resources lên 1 giây sau lỗi cùng crop size để tránh thử cấp phát ở mỗi frame; crop size mới vẫn thử ngay. Chỉ sửa source, chưa build DLL.
- OBS sender tối ưu vòng BGRA→RGB fallback encoder bằng con trỏ tăng dần, giữ nguyên thứ tự kênh, tránh phép nhân/chỉ số mỗi pixel; chưa build DLL theo yêu cầu.
- Pixel Seek đổi dấu `visited` flood-fill sang bitmap 1-bit/pixel với epoch 32-bit theo word: tổng scratch dấu vết còn 3/16 byte mỗi pixel và không còn xóa toàn mask mỗi 255 frame; có regression source cho epoch chuyển qua 255, chưa chạy test.
- Bảo đảm generation 32-bit wrap xóa toàn bộ epoch metadata, kể cả word của frame lớn trước đó sau khi độ phân giải giảm; thêm regression source, chưa chạy test.
- Chỉ chỉnh source tĩnh theo yêu cầu khi người dùng đang chơi: không mở app/game/OBS, không build và không chạy test. Windows packaging, DLL build và hành vi trên máy thật chưa được xác minh cho các thay đổi hiện tại.

## Ghi chú lịch sử — 2026-10-16 (Web Config đã bị loại bỏ)

- Đã hoàn thiện profile settings trong backend và Web Config: khởi đầu với `Default`, tối đa ba profile, tạo, clone, đổi tên, xoá và chọn profile. Mỗi profile giữ snapshot setting riêng trong `app-settings.json`; nút chuột giữa chỉ chuyển profile một lần theo rising edge.
- Receiver monitor hiện hiển thị tên profile đang chọn trong header. Tên và index lấy từ state WebSocket (`profile_index`/`profiles`), vì vậy Receiver tự đổi theo thao tác trên Web Config hoặc lần chuyển profile bằng chuột giữa mà không cần khởi động lại.
- Auto target gate dùng mask HSV chung với Motion, chỉ báo target hợp lệ khi pixel màu chạm tâm crosshair trong Auto FOV; pixel màu ở chỗ khác không kích hoạt, không chọn target chuyển động và không vẽ line/ring tới pixel Auto. Auto state machine bắt đầu Delay từ lần thấy pixel chạm tâm, bắt đầu Cooldown khi phát lệnh click, dùng Sleep làm nhịp polling riêng, rồi gửi left-click qua Medius khi đủ điều kiện.
- Đã cập nhật README, AGENTS, DEVELOPMENT và `docs/FEATURES_AND_CHANGES.md` theo source hiện tại. Kiểm thử cuối được ghi ở commit mới; Windows, OBS LAN và thiết bị Medius thật vẫn chưa được xác thực trong môi trường macOS.

## Cập nhật thiết bị và trạng thái tính năng

- Chỉ còn Medius qua Rust SDK `medius 3.4.6`; quét CH343 control port rồi `Device::open` trước khi kiểm tra health, nên nhận box không phụ thuộc clone chuột đã enumerate xong sau USB reconnect; chuẩn hóa adapter output, thay tài liệu Native wire bằng Rust library, bỏ Ferrum và không tích hợp MAKCU. Thuật toán tracking và Delay/Cooldown/Sleep không đổi.
- Pixel Seek là chế độ dùng được duy nhất. Object Detection và ONNX Runtime/OpenVINO/TensorRT được đánh dấu chưa dùng được; UI khóa các lựa chọn đó.
- Sửa lỗi Apply Changes bị deadlock trong lệnh cấu hình sensitivity/motion. Windows và thiết bị thật vẫn cần kiểm tra.

## 1. Skill

- Đã cài 7 design skill từ `~/.codex/skills` vào `~/.claude/skills`: `impeccable`, `ui-ux-pro-max`, `design`, `design-system`, `ui-styling`, `no-ai-design-slop`, `design-first-ui-prompting`.
- `ponytail` không có trong thư mục codex; Claude có sẵn bản `anthropic-skills:ponytail` và đã dùng nó.

## 2. Receiver UI — bỏ egui, chuyển sang web monitor

- Xoá cửa sổ `eframe/egui`. Host native (`src-tauri`) giờ chỉ làm phần bắt buộc phải native: chạy backend nhúng, đọc Medius, gửi chuyển động chuột.
- Receiver là trang `/monitor.html` (Leptos/WASM), host mở bằng Edge `--app` (cửa sổ không thanh địa chỉ, profile riêng tại `%LOCALAPPDATA%\SysX\monitor-window`, cỡ mặc định 550 × 620). Không có Edge thì mở bằng trình duyệt mặc định.
- Đóng cửa sổ monitor → SysX tự tắt sau 3 giây (đã kiểm tra trên máy: backend tắt, UDP 4200 được giải phóng). Thêm retry khi xoá thư mục runtime vì Windows có lúc để lại thư mục rỗng.
- Trạng thái thiết bị và 5 nút chuột: host → backend (`device_status`, có kiểm tra hợp lệ) → `state.device` → trang monitor.
- Backend không gửi frame preview cũ hơn 1 giây, nên mở lại trang khi sender đã dừng sẽ không hiện ảnh cũ và báo nhầm "Receiving frames".
- Thiết kế (theo Impeccable / UI UX Pro Max / no-AI-slop):
  - Header mỏng: logo SysX bên trái, chỉ trạng thái thiết bị bên phải (đã bỏ "Waiting for sender" và "UDP 4200" theo yêu cầu).
  - Preview vuông tối đa 640px, HUD `độ phân giải · FPS · ms` nằm trong khung.
  - Khi chưa có frame: `NO FRAME` + dòng trạng thái; chỉ hiện dòng chi tiết khi có lỗi (đã bỏ "Listening on UDP 4200").
  - Hàng 5 nút chuột rộng bằng preview; giữ = chấm đặc xanh, nhả = vòng tròn, chưa biết = gạch ngang.
  - Monospace chỉ dùng cho số đo; không có animation trang trí.
- Ảnh kiểm tra (chụp bằng Edge headless, không mở cửa sổ): `design-review/receiver-monitor-web-{live,waiting,narrow}.png`. Hai thay đổi cuối (bỏ trạng thái/port ở header và dòng "Listening") đã build nhưng chưa chụp lại.

## 3. OBS plugin (`obs-plugin/sysx-canvas-sender`)

- Cấu hình nằm trong code (theo yêu cầu): sửa các hằng số đầu file `sysx_canvas_sender.cpp` rồi build lại.
  - `kReceiverHost = "127.0.0.1"`, `kReceiverPort = 4200`, `kJpegQuality = 80`, `kMaxFps = 0`, `kChroma444 = true`, `kDefaultCrop = 640`.
- Không ghi log vào OBS (theo yêu cầu).
- DLL không cần gì thêm: libjpeg-turbo và VC++ runtime link tĩnh; DLL chỉ phụ thuộc `KERNEL32.dll` và `WS2_32.dll` của Windows, các hàm OBS lấy từ `obs.dll` đang chạy.
- Crop trên GPU theo lệnh `SYSCFG 1 CROP <n>` từ receiver (chỉ nhận từ đúng receiver đã cấu hình), staging 3 surface để không chặn luồng render của OBS.
- Encode JPEG từ BGRA trực tiếp, chroma 4:4:4 để màu Pixel Seek không bị nhoè; buffer gửi UDP 8 MiB.
- Canvas HDR (không phải BGRA 8-bit) được bỏ qua, không crash.
- DLL đã build: `obs-plugin/sysx-canvas-sender/sysx-canvas-sender.dll`; đã cài vào `C:\Program Files\obs-studio\obs-plugins\64bit\`, DLL cũ đổi tên thành `sysx-canvas-sender-gpu-localhost.dll.bak`.

### Kết quả kiểm tra plugin

| Kiểm tra | Kết quả |
|---|---|
| `sysx-sender-selftest` → `udp-listen` | 3/3 frame (320×320 nhiều chunk, 200×120 có padding, 1×1), 0 lỗi/mất, nhận `SYSCFG` crop 640 |
| `sysx-obs-host-check`: plugin chạy trong `obs.dll` OBS 32.2.2 thật, D3D11, không cửa sổ, 60 FPS | 300/300 frame 640×640, unload sạch |
| Canvas HDR (Rec.2100 PQ) | không gửi frame, không lỗi |
| Render 240 FPS / 360 FPS | nhận 234,2 / 356,8 FPS (loopback, 4:4:4, quality 80) |
| Bản không log | 239 frame / 4 giây ở 60 FPS, không có dòng log nào của plugin |

## 4. Build và test

- `scripts/build-windows.ps1` build toàn bộ không cần Node: Trunk UI → backend → test 3 crate → `SysX.exe` → smoke UDP. `-AppSmoke` (mở cửa sổ) là tuỳ chọn.
- Sửa smoke script cho Windows PowerShell 5.1 (`ExitCode` rỗng nếu không giữ handle).
- Test: backend 37, UI 5, host 8 — đều pass. Smoke UDP 4200 pass.

## 5. Chưa làm

- Chạy plugin trong giao diện OBS Studio thật cùng SysX (bị dừng vì bạn đang chơi game).
- Chạy `scripts/build-windows.ps1 -AppSmoke`.
- Gửi qua LAN giữa hai máy (chỉ có một máy).
- Chụp lại monitor sau khi bỏ trạng thái/port ở header.
