"""Static ncurses for MySQL's bundled editline client."""

load("@rules_foreign_cc//foreign_cc:defs.bzl", "configure_make")

package(default_visibility = ["//visibility:public"])

filegroup(
    name = "all_srcs",
    srcs = glob(["**"], exclude = ["bazel-*/**"]),
)

configure_make(
    name = "ncurses",
    args = ["-j8"],
    configure_in_place = True,
    configure_options = [
        "--disable-db-install",
        "--disable-termcap",
        "--enable-pc-files",
        "--without-ada",
        "--without-cxx",
        "--without-cxx-binding",
        "--without-debug",
        "--without-manpages",
        "--without-progs",
        "--without-shared",
        "--without-tests",
    ],
    lib_source = ":all_srcs",
    out_static_libs = ["libncursesw.a"],
)
