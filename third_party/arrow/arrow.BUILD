"""Minimal native Arrow C++ Flight build for yesno's C++ and MySQL edges."""

load("@rules_foreign_cc//foreign_cc:defs.bzl", "cmake")

package(default_visibility = ["//visibility:public"])

filegroup(
    name = "cpp_srcs",
    srcs = [
        ".env",
        "LICENSE.txt",
        "NOTICE.txt",
    ] + glob([
        "cpp/**",
        "format/**",
    ]),
)

cmake(
    name = "arrow_cpp",
    build_args = ["-j8"],
    build_data = [
        "@arrow_absl_20250127//file",
        "@arrow_cares_1_34_6//file",
        "@arrow_grpc_1_76_0//file",
        "@arrow_protobuf_31_1//file",
        "@arrow_re2_2023_03_01//file",
        "@arrow_xsimd_14_2_0//file",
        "@arrow_zlib_1_3_1//file",
    ],
    cache_entries = {
        "ARROW_BUILD_EXAMPLES": "OFF",
        "ARROW_BUILD_SHARED": "ON",
        "ARROW_BUILD_STATIC": "OFF",
        "ARROW_BUILD_TESTS": "OFF",
        "ARROW_COMPUTE": "OFF",
        "ARROW_CSV": "OFF",
        "ARROW_DATASET": "OFF",
        "ARROW_DEPENDENCY_SOURCE": "BUNDLED",
        "ARROW_FILESYSTEM": "OFF",
        "ARROW_FLIGHT": "ON",
        "ARROW_JSON": "OFF",
        "ARROW_MIMALLOC": "OFF",
        "ARROW_PARQUET": "OFF",
        "ARROW_RUNTIME_SIMD_LEVEL": "NONE",
        "ARROW_SIMD_LEVEL": "NONE",
        "ARROW_WITH_BROTLI": "OFF",
        "ARROW_WITH_BZ2": "OFF",
        "ARROW_WITH_LZ4": "OFF",
        "ARROW_WITH_SNAPPY": "OFF",
        "ARROW_WITH_ZSTD": "OFF",
        "CMAKE_CXX_FLAGS": "-std=c++17",
        "CMAKE_CXX_STANDARD_LIBRARIES": "-lstdc++ -lm",
    },
    env = {
        "ARROW_ABSL_URL": "file://$$EXT_BUILD_ROOT/$(location @arrow_absl_20250127//file)",
        "ARROW_CARES_URL": "file://$$EXT_BUILD_ROOT/$(location @arrow_cares_1_34_6//file)",
        "ARROW_GRPC_URL": "file://$$EXT_BUILD_ROOT/$(location @arrow_grpc_1_76_0//file)",
        "ARROW_PROTOBUF_URL": "file://$$EXT_BUILD_ROOT/$(location @arrow_protobuf_31_1//file)",
        "ARROW_RE2_URL": "file://$$EXT_BUILD_ROOT/$(location @arrow_re2_2023_03_01//file)",
        "ARROW_XSIMD_URL": "file://$$EXT_BUILD_ROOT/$(location @arrow_xsimd_14_2_0//file)",
        "ARROW_ZLIB_URL": "file://$$EXT_BUILD_ROOT/$(location @arrow_zlib_1_3_1//file)",
    },
    install = True,
    lib_source = ":cpp_srcs",
    working_directory = "cpp",
    out_data_dirs = [
        "lib/cmake/Arrow",
        "lib/cmake/ArrowFlight",
    ],
    out_include_dir = "include",
    out_shared_libs = [
        "libarrow.so",
        "libarrow_flight.so",
    ],
)
