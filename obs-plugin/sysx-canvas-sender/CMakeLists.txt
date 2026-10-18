cmake_minimum_required(VERSION 3.20)
# Static CRT: the DLL needs no Visual C++ redistributable, only Windows and obs.dll.
set(CMAKE_MSVC_RUNTIME_LIBRARY "MultiThreaded$<$<CONFIG:Debug>:Debug>")
project(sysx_canvas_sender LANGUAGES CXX)

set(SYSX_RECEIVER_IP "192.168.88.59" CACHE STRING "Receiver IPv4 address")
if(NOT SYSX_RECEIVER_IP MATCHES "^[0-9]+\\.[0-9]+\\.[0-9]+\\.[0-9]+$")
  message(FATAL_ERROR "SYSX_RECEIVER_IP must be an IPv4 address")
endif()
string(REPLACE "." ";" _sysx_ip_octets "${SYSX_RECEIVER_IP}")
foreach(_octet IN LISTS _sysx_ip_octets)
  if(_octet GREATER 255)
    message(FATAL_ERROR "SYSX_RECEIVER_IP octet exceeds 255")
  endif()
endforeach()

if(NOT WIN32)
  message(FATAL_ERROR "sysx-canvas-sender is a Windows OBS module")
endif()

set(OBS_INCLUDE_DIR "" CACHE PATH "libobs include directory (obs-studio/libobs plus a directory with obsconfig.h)")
set(JPEG_INCLUDE_DIR "" CACHE PATH "libjpeg-turbo include directory")
set(JPEG_LIBRARY "" CACHE FILEPATH "libjpeg-turbo library; prefer jpeg-static.lib so OBS needs no jpeg62.dll")

find_path(OBS_INCLUDE_DIR obs-module.h REQUIRED)
find_path(JPEG_INCLUDE_DIR jpeglib.h REQUIRED)
if(NOT JPEG_LIBRARY)
  find_library(JPEG_LIBRARY NAMES jpeg-static jpeg REQUIRED)
endif()

add_library(sysx-canvas-sender MODULE sysx_canvas_sender.cpp)
target_compile_features(sysx-canvas-sender PRIVATE cxx_std_17)
# C4201: nameless unions in libobs headers. C4324/C4611: jmp_buf alignment and
# setjmp in the libjpeg error path, which holds no non-trivial local objects.
target_compile_options(sysx-canvas-sender PRIVATE /W4 /EHsc /wd4201 /wd4324 /wd4611)
target_link_options(sysx-canvas-sender PRIVATE /DEBUG:NONE /OPT:REF /OPT:ICF)
target_compile_definitions(sysx-canvas-sender PRIVATE SYSX_RECEIVER_HOST="${SYSX_RECEIVER_IP}")
target_include_directories(sysx-canvas-sender PRIVATE ${OBS_INCLUDE_DIR} "${JPEG_INCLUDE_DIR}")
target_link_libraries(sysx-canvas-sender PRIVATE "${JPEG_LIBRARY}" ws2_32)
target_compile_definitions(sysx-canvas-sender PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN _CRT_SECURE_NO_WARNINGS)
set_target_properties(sysx-canvas-sender PROPERTIES PREFIX "" SUFFIX ".dll" OUTPUT_NAME "sysx-canvas-sender")

# Headless protocol self-test (no OBS needed); see main() in the source.
add_executable(sysx-sender-selftest EXCLUDE_FROM_ALL sysx_canvas_sender.cpp)
target_compile_features(sysx-sender-selftest PRIVATE cxx_std_17)
target_compile_options(sysx-sender-selftest PRIVATE /W4 /EHsc /wd4201 /wd4324 /wd4611)
target_include_directories(sysx-sender-selftest PRIVATE ${OBS_INCLUDE_DIR} "${JPEG_INCLUDE_DIR}")
target_link_libraries(sysx-sender-selftest PRIVATE "${JPEG_LIBRARY}" ws2_32)
target_compile_definitions(sysx-sender-selftest PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN _CRT_SECURE_NO_WARNINGS SYSX_SENDER_SELFTEST)

# Headless libobs host: loads the built plugin inside the real obs.dll with
# D3D11 video and no window; see obs_host_check.cpp.
add_executable(sysx-obs-host-check EXCLUDE_FROM_ALL obs_host_check.cpp)
target_compile_features(sysx-obs-host-check PRIVATE cxx_std_17)
target_compile_options(sysx-obs-host-check PRIVATE /W4 /EHsc /wd4201)
target_include_directories(sysx-obs-host-check PRIVATE ${OBS_INCLUDE_DIR})
target_compile_definitions(sysx-obs-host-check PRIVATE NOMINMAX WIN32_LEAN_AND_MEAN)
