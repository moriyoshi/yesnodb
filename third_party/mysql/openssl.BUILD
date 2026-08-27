"""Shared OpenSSL matching MySQL 8.4.0's supported 3.0 release."""

load("@rules_foreign_cc//foreign_cc:defs.bzl", "configure_make")

package(default_visibility = ["//visibility:public"])

filegroup(
    name = "all_srcs",
    srcs = glob(["**"], exclude = ["bazel-*/**"]),
)

configure_make(
    name = "openssl",
    args = ["-j8"],
    configure_command = "config",
    configure_in_place = True,
    configure_options = [
        "no-module",
        "no-tests",
    ],
    lib_source = ":all_srcs",
    out_shared_libs = [
        "libssl.so",
        "libcrypto.so",
    ],
    targets = [
        "build_libs",
        "install_sw",
    ],
)
