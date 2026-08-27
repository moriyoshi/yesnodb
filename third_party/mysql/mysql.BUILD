"""Overlay applied to the pinned MySQL 8.4.0 source distribution."""

load("@rules_foreign_cc//foreign_cc:defs.bzl", "cmake")

package(default_visibility = ["//visibility:public"])

filegroup(
    name = "all_srcs",
    srcs = glob(["**"], exclude = ["bazel-*/**"]),
)

cmake(
    name = "mysql",
    build_args = ["-j8"],
    build_data = [
        "@@//yesno-c:yesno_c",
        "@@//yesno-c:yesno_header",
    ],
    cache_entries = {
        # rules_foreign_cc's host GCC toolchain uses the gcc driver for C++ too
        # and appends its own C++17 flag. MySQL 8.4 requires C++20, so the final
        # standard flag, C++ runtime, and trailing math library all have to be explicit.
        "CMAKE_C_FLAGS": "-I$$EXT_BUILD_DEPS/ncurses/include",
        "CMAKE_CXX_FLAGS": "-std=c++20 -I$$EXT_BUILD_DEPS/ncurses/include",
        "CMAKE_CXX_STANDARD_LIBRARIES": "-lstdc++ -lm",
        "CMAKE_PREFIX_PATH": "$$EXT_BUILD_DEPS/arrow_cpp",
        "CURSES_INCLUDE_PATH": "$$EXT_BUILD_DEPS/ncurses/include/ncursesw",
        "CURSES_LIBRARY": "$$EXT_BUILD_DEPS/ncurses/lib/libncursesw.a",
        "PATCHELF_EXECUTABLE": "$$EXT_BUILD_DEPS/patchelf/bin/patchelf",
        "WITH_CURL": "none",
        "WITH_NDB": "OFF",
        "WITH_ROUTER": "OFF",
        "WITH_SSL": "$$EXT_BUILD_DEPS/openssl",
        "WITH_UNIT_TESTS": "OFF",
        "YESNO_WITH_FLIGHT": "ON",
        "WITH_YESNO_STORAGE_ENGINE": "0",
        "WITHOUT_GROUP_REPLICATION": "ON",
        "YESNO_BAZEL_BUNDLE": "ON",
        "YESNO_C_HEADER": "$$EXT_BUILD_ROOT/$(location @@//yesno-c:yesno_header)",
        "YESNO_C_PREBUILT_LIBRARY": "$$EXT_BUILD_ROOT/$(location @@//yesno-c:yesno_c)",
    },
    deps = [
        "@apache_arrow_25_0_1//:arrow_cpp",
        "@ncurses_6_5//:ncurses",
        "@openssl_3_0_13//:openssl",
        "@patchelf_0_18_0//:patchelf",
    ],
    install = True,
    lib_source = ":all_srcs",
    out_binaries = [
        "mysqld",
        "mysql",
        "mysqltest",
    ],
    out_data_dirs = [
        "share",
        "lib/private",
    ],
    out_lib_dir = "lib/plugin",
    out_shared_libs = ["ha_yesno.so"],
)
