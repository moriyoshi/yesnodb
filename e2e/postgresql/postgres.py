# PostgreSQL E2E through the ordinary yesno-e2e world and generic fixture verbs.
# Bazel supplies only immutable, ABI-pinned resources; every mutable path and
# process belongs to this scenario's temporary world.

pg_source = fx_resource("PG_HOME")
ext_lib = fx_resource("EXT_LIB")
ext_control = fx_resource("EXT_CONTROL")
ext_sql = fx_resource("EXT_SQL")
sql_dir = fx_resource("SQL_DIR")
expected_dir = fx_resource("EXPECTED_DIR")
isolation_dir = fx_resource("ISOLATION_DIR")

work = fx_temp("postgresql")
pg = fx_join(work, "prefix")
data = fx_join(work, "data")
socket = fx_join(work, "socket")
fx_copy_tree(pg_source, pg)
fx_mkdir(socket)

bin_dir = fx_join(pg, "bin")
lib_dir = fx_join(pg, "lib")
extension_dir = fx_join(pg, "share/extension")
initdb = fx_join(bin_dir, "initdb")
postgres = fx_join(bin_dir, "postgres")
psql = fx_join(bin_dir, "psql")
fx_copy(ext_lib, fx_join(lib_dir, "yesno_pg.so"), 493)
fx_copy(ext_control, fx_join(extension_dir, "yesno_pg.control"), 420)
fx_copy(ext_sql, fx_join(extension_dir, "yesno_pg--0.1.0.sql"), 420)

loader = "LD_LIBRARY_PATH=" + lib_dir
env = [
    loader,
    "PATH=" + bin_dir,
    "PGHOST=" + socket,
    "PGUSER=postgres",
    "PGDATABASE=postgres",
]


def checked(out, label):
    assert out["success"], f"{label} failed:\n{actual}\n{out['stderr']}"
    return out


checked(
    fx_run(
        initdb,
        ["-D", data, "-U", "postgres", "--no-sync", "--encoding=UTF8", "--locale=C"],
        env,
        "",
    ),
    "initdb",
)
conf = fx_join(data, "postgresql.conf")
fx_write(
    conf,
    fx_read(conf)
    + "\nlisten_addresses = ''\n"
    + "unix_socket_directories = '"
    + socket
    + "'\nfsync = off\nfull_page_writes = off\n",
)
server = fx_start(postgres, ["-D", data], env)
fx_wait_ready(server, psql, ["-X", "-q", "-c", "SELECT 1"], env, 60000)
assert fx_alive(server)

# The shared in-process Flight primitive removes the need for a helper server binary.
seed_keys = [42, 42, 42, 42, 42, 42, 42, 42, 42, 42]
seed_ords = [0, 7, 14, 21, 28, 35, 42, 49, 56, 63]
for ordinal in [0, 3, 6, 9, 12, 15, 18, 21, 24, 27]:
    seed_keys.append(43)
    seed_ords.append(ordinal)
for ordinal in [0, 9223372036854775808, 9223372036854775809, 18446744073709551614]:
    seed_keys.append(7)
    seed_ords.append(ordinal)
endpoint = fx_flight_start(seed_keys, seed_ords)
assert endpoint.startswith("http://127.0.0.1:")


def psql_run(args, stdin=""):
    return fx_run_merged(psql, ["-X"] + args, env, stdin)


# A direct smoke assertion keeps the fixture from passing merely because all
# expected files happened to be empty or skipped.
checked(psql_run(["-q", "-c", "CREATE EXTENSION yesno_pg"]), "create extension")
version = checked(psql_run(["-At", "-c", "SELECT yesno_pg_version()"]), "version")
assert version["stdout"] == "0.1.0\n"
checked(psql_run(["-q", "-c", "DROP EXTENSION yesno_pg CASCADE"]), "drop extension")

sql_files = fx_list(sql_dir, ".sql")
expected_files = fx_list(expected_dir, ".out")
assert len(sql_files) > 0


def expected_for(name):
    suffix = "/" + name + ".out"
    for candidate in expected_files:
        if candidate.endswith(suffix):
            return candidate
    assert False, f"no expected output for {name}"


for sql_file in sql_files:
    filename = sql_file.rsplit("/", 1)[-1]
    name = filename[:-4]
    out = psql_run(["-a", "-q", "-v", "endpoint=" + endpoint, "-f", sql_file])
    checked(out, name)
    actual = out["stdout"]
    expected = fx_read(expected_for(name))
    assert actual == expected, (
        f"{name} differs\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
    )
    assert fx_alive(server), f"PostgreSQL died during {name}: {out['stderr']}"


def session(label):
    session_env = env + ["PGOPTIONS=-c yesno_pg.endpoint=" + endpoint]
    return fx_start_tty(psql, ["-X", "-a", "-q"], session_env)


def send(session_handle, label, sequence, sql):
    marker = f"--__YNSYNC_{label}_{sequence}__\n"
    fx_send(session_handle, sql + "\n" + marker)
    return marker


def wait_body(session_handle, marker, sql):
    chunk = fx_wait_output(session_handle, marker, 60000)
    echoed = sql + "\n"
    assert chunk.startswith(echoed), f"psql did not echo {sql}: {chunk}"
    return chunk[len(echoed) :]

specs = fx_list(isolation_dir, ".spec")
for spec in specs:
    filename = spec.rsplit("/", 1)[-1]
    name = filename[:-5]
    a = session("a")
    b = session("b")
    seq_a = 0
    seq_b = 0
    pending_a = None
    pending_b = None
    actual = ""
    for raw in fx_read(spec).splitlines():
        line = raw.strip()
        if line == "" or line.startswith("#"):
            continue
        if line == "A!":
            assert pending_a is not None
            actual += wait_body(a, pending_a[0], pending_a[1])
            pending_a = None
        elif line == "B!":
            assert pending_b is not None
            actual += wait_body(b, pending_b[0], pending_b[1])
            pending_b = None
        elif line.startswith("A&: "):
            assert pending_a is None
            sql = line[4:]
            seq_a += 1
            actual += "A: " + sql + "\n"
            pending_a = [send(a, "A", seq_a, sql), sql]
        elif line.startswith("B&: "):
            assert pending_b is None
            sql = line[4:]
            seq_b += 1
            actual += "B: " + sql + "\n"
            pending_b = [send(b, "B", seq_b, sql), sql]
        elif line.startswith("A: "):
            assert pending_a is None
            sql = line[3:]
            seq_a += 1
            actual += "A: " + sql + "\n"
            actual += wait_body(a, send(a, "A", seq_a, sql), sql)
        elif line.startswith("B: "):
            assert pending_b is None
            sql = line[3:]
            seq_b += 1
            actual += "B: " + sql + "\n"
            actual += wait_body(b, send(b, "B", seq_b, sql), sql)
        else:
            assert False, f"invalid isolation directive: {line}"
    assert pending_a is None and pending_b is None
    checked(fx_close(a, 10000), name + " session A")
    checked(fx_close(b, 10000), name + " session B")
    expected = fx_read(expected_for(name))
    assert actual == expected, (
        f"{name} differs\n--- expected ---\n{expected}\n--- actual ---\n{actual}"
    )
    assert fx_alive(server), f"PostgreSQL died during {name}"

fx_flight_stop()
fx_stop(server)
