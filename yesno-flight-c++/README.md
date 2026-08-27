# yesno-flight-c++

`yesno-flight-c++` is a synchronous, host-independent C++ client for yesno's
Arrow Flight protocol. It uses Apache Arrow's native C++ `FlightClient`; it
does not route through the Rust client or a C ABI.

The client exposes:

- versioned `GetFlightInfo` planning and snapshot-ticket fetches;
- exact cardinality without fetching rows;
- validated, strictly ascending `UInt64` ordinal materialization;
- atomic point insert, remove, contains, and whole-key clear actions;
- Arrow `RecordBatch` bulk insert and remove.

One `Client` is synchronous and must not be invoked concurrently. A host may
serialize access, as `yesno-mysql` does, or create one client per thread.

## Build

Apache Arrow C++ with Flight support must be installed. Point CMake at that
prefix if it is not in the default package search path:

```console
cmake -S yesno-flight-c++ \
  -B .agents-workspace/tmp/yesno-flight-cpp-build \
  -DCMAKE_PREFIX_PATH=/path/to/arrow
cmake --build .agents-workspace/tmp/yesno-flight-cpp-build --parallel
ctest --test-dir .agents-workspace/tmp/yesno-flight-cpp-build --output-on-failure
```

The repository's authoritative native build uses the sha256-pinned Arrow
25.0.1 source and all of its declared Flight dependencies:

```console
bazelisk build //yesno-flight-c++:yesno_flight_cpp
bazelisk test //yesno-flight-c++:unit_tests
bazelisk build //yesno-flight-c++:roundtrip
```

The hermetic unit target needs no server. It covers fixed-width action framing,
strict scalar and boolean response decoding, the bulk pair schema, ordinal
batch shape/null/order validation across batch boundaries, and exact promised
cardinality. `roundtrip` is the separate live-server integration executable.

The live roundtrip executable accepts an Arrow Flight URI:

```console
.agents-workspace/tmp/yesno-flight-cpp-build/yesno_flight_cpp_roundtrip \
  grpc://127.0.0.1:50051
```

The server must implement yesno's Flight vocabulary. In particular, the
point-mutation actions report whether the set changed, which preserves SQL
duplicate-key and missing-row semantics without a racy contains-then-write
sequence.
