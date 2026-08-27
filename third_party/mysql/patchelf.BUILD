"""Pinned patchelf used by MySQL's custom-OpenSSL RPATH fixups."""

load("@rules_foreign_cc//foreign_cc:defs.bzl", "configure_make")

package(default_visibility = ["//visibility:public"])

filegroup(
    name = "all_srcs",
    srcs = glob(["**"], exclude = ["bazel-*/**"]),
)

configure_make(
    name = "patchelf",
    autoreconf = True,
    autoreconf_options = ["--install"],
    configure_in_place = True,
    configure_options = ["--disable-dependency-tracking"],
    lib_source = ":all_srcs",
    out_binaries = ["patchelf"],
    targets = [
        "",
        "install",
    ],
)
