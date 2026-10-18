# libopus for the phone's audio: decoding the host's stream and encoding the mic.
# MediaCodec's Opus path cost ~35% of a core for 100 small packets a second;
# libopus does the same work in about 1%.
cmake_minimum_required(VERSION 3.18)
project(dsopus C)

include(FetchContent)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)
set(OPUS_BUILD_SHARED_LIBRARY OFF CACHE BOOL "" FORCE)
set(OPUS_BUILD_PROGRAMS OFF CACHE BOOL "" FORCE)
set(OPUS_BUILD_TESTING OFF CACHE BOOL "" FORCE)
set(OPUS_INSTALL_PKG_CONFIG_MODULE OFF CACHE BOOL "" FORCE)
set(OPUS_INSTALL_CMAKE_CONFIG_MODULE OFF CACHE BOOL "" FORCE)
# Pinned to the hash xiph.org publishes in SHA256SUMS.txt.
FetchContent_Declare(
    opus
    URL https://downloads.xiph.org/releases/opus/opus-1.5.2.tar.gz
    URL_HASH SHA256=65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1
)
FetchContent_MakeAvailable(opus)

add_library(dsopus SHARED dsopus_jni.c)
target_link_libraries(dsopus PRIVATE opus)
target_compile_options(dsopus PRIVATE -O2 -Wall)
# 16 KB page alignment, required for new apps on Android 15+ devices.
target_link_options(dsopus PRIVATE "-Wl,-z,max-page-size=16384")
