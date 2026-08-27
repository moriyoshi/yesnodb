"""Pinned MySQL source repository with the local YESNO plugin overlaid."""

def _remove_upstream_build_files(ctx):
    # A foreign_cc filegroup must see the source as one tree. Upstream bundles
    # Bazel metadata for several vendored libraries; leaving those files in
    # place turns their directories into subpackages and silently removes the
    # ICU/protobuf/Abseil sources from the CMake action.
    result = ctx.execute([
        "find",
        ctx.path("."),
        "-type",
        "f",
        "(",
        "-name",
        "BUILD",
        "-o",
        "-name",
        "BUILD.bazel",
        ")",
    ])
    if result.return_code != 0:
        fail("cannot enumerate upstream BUILD files: " + result.stderr)
    for path in result.stdout.splitlines():
        ctx.delete(path)


def _mysql_repository_impl(ctx):
    ctx.download_and_extract(
        url = ctx.attr.urls,
        sha256 = ctx.attr.sha256,
        stripPrefix = ctx.attr.strip_prefix,
    )
    ctx.patch(ctx.path(ctx.attr.mysql_patch), strip = 1)
    _remove_upstream_build_files(ctx)
    ctx.symlink(ctx.path(ctx.attr.build_file), "BUILD.bazel")
    ctx.file("storage/yesno/.bazel-overlay", "")
    ctx.symlink(ctx.path(ctx.attr.plugin_cmake), "storage/yesno/CMakeLists.txt")
    ctx.symlink(ctx.path(ctx.attr.backend_header), "storage/yesno/backend.h")
    ctx.symlink(ctx.path(ctx.attr.embedded_backend_source), "storage/yesno/backend_embedded.cc")
    ctx.symlink(ctx.path(ctx.attr.flight_backend_source), "storage/yesno/backend_flight.cc")
    ctx.symlink(ctx.path(ctx.attr.plugin_source), "storage/yesno/ha_yesno.cc")
    ctx.symlink(ctx.path(ctx.attr.plugin_header), "storage/yesno/ha_yesno.h")
    ctx.symlink(ctx.path(ctx.attr.flight_cpp_cmake), "storage/yesno/flight-cpp/CMakeLists.txt")
    ctx.symlink(ctx.path(ctx.attr.flight_cpp_header), "storage/yesno/flight-cpp/include/yesno/flight/client.h")
    ctx.symlink(ctx.path(ctx.attr.flight_cpp_internal_header), "storage/yesno/flight-cpp/src/protocol_internal.h")
    ctx.symlink(ctx.path(ctx.attr.flight_cpp_internal_source), "storage/yesno/flight-cpp/src/protocol_internal.cc")
    ctx.symlink(ctx.path(ctx.attr.flight_cpp_source), "storage/yesno/flight-cpp/src/client.cc")

mysql_repository = repository_rule(
    implementation = _mysql_repository_impl,
    attrs = {
        "build_file": attr.label(mandatory = True, allow_single_file = True),
        "backend_header": attr.label(mandatory = True, allow_single_file = True),
        "embedded_backend_source": attr.label(mandatory = True, allow_single_file = True),
        "flight_backend_source": attr.label(mandatory = True, allow_single_file = True),
        "flight_cpp_cmake": attr.label(mandatory = True, allow_single_file = True),
        "flight_cpp_header": attr.label(mandatory = True, allow_single_file = True),
        "flight_cpp_internal_header": attr.label(mandatory = True, allow_single_file = True),
        "flight_cpp_internal_source": attr.label(mandatory = True, allow_single_file = True),
        "flight_cpp_source": attr.label(mandatory = True, allow_single_file = True),
        "plugin_cmake": attr.label(mandatory = True, allow_single_file = True),
        "plugin_header": attr.label(mandatory = True, allow_single_file = True),
        "plugin_source": attr.label(mandatory = True, allow_single_file = True),
        "mysql_patch": attr.label(mandatory = True, allow_single_file = True),
        "sha256": attr.string(mandatory = True),
        "strip_prefix": attr.string(mandatory = True),
        "urls": attr.string_list(mandatory = True),
    },
)
