# yesno-c

`yesno-c` is the host-independent C ABI for embedding yesnodb. It is the layer
a database plugin can use without importing Rust types or adopting a
process-global database.

The API exposes two opaque handles:

- `yesno_db` owns a durable `yesno_core::Db` and may be shared by caller-managed
  threads. Do not race `yesno_db_close` with another operation.
- `yesno_cursor` owns an immutable, materialized snapshot of one key's ordinal
  set. Writes made after the cursor opens do not change its rows. Cursor
  operations must not be invoked concurrently.

Every fallible call returns `YESNO_OK` or `YESNO_ERROR`. An optional caller-owned
error buffer receives a NUL-terminated message. Boolean outputs use `uint8_t` so
their ABI does not depend on the C or C++ representation of `bool`.

## Build

```console
CARGO_TARGET_DIR=.agents-workspace/tmp/yesno-c-target \
  cargo build --release --manifest-path yesno-c/Cargo.toml --locked
```

The build produces both libraries:

```text
.agents-workspace/tmp/yesno-c-target/release/libyesno_c.a
.agents-workspace/tmp/yesno-c-target/release/libyesno_c.so
```

Include [`include/yesno.h`](include/yesno.h) and link one of those artifacts.
For the static library on Linux, also link `pthread`, `dl`, and `m`.

## Example

```c
#include "yesno.h"

#include <stdint.h>
#include <stdio.h>

int main(void) {
  char error[256];
  yesno_db *db = NULL;
  if (yesno_db_open("./yesno-data", &db, error, sizeof(error)) != YESNO_OK) {
    fprintf(stderr, "%s\n", error);
    return 1;
  }

  uint8_t changed = 0;
  if (yesno_db_insert(db, 42, 7, &changed, error, sizeof(error)) != YESNO_OK) {
    fprintf(stderr, "%s\n", error);
    yesno_db_close(db);
    return 1;
  }

  yesno_db_close(db);
  return changed == 1 ? 0 : 2;
}
```

The ordinal domain is `[0, 2^64 - 2]`; `UINT64_MAX` is reserved and every API
that names it returns an error.

## Test

```console
./yesno-c/gate.sh
```

The Rust properties guard error-buffer bounds with a trailing canary and check
every cursor seek mode against sorted-set partitioning. The gate also lints
Rust, builds the static library, compiles
[`tests/smoke.c`](tests/smoke.c) as strict C11, and runs it against two distinct
durable databases. The smoke test covers isolation between handles, duplicate
inserts, the reserved ordinal, cursor seeks and stable EOF, snapshot isolation,
checkpoint/reopen persistence, clear, and one-byte error buffers.
