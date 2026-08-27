# MySQL E2E through the ordinary yesno-e2e world and the same generic fixture
# verbs used by PostgreSQL. Both the embedded C ABI backend and the native
# Arrow Flight backend run against the exact MySQL build supplied by Bazel.

mysql_source = fx_resource("MYSQL_HOME")
test_sql = fx_resource("TEST_SQL")
expected_file = fx_resource("EXPECTED")


def checked(out, label):
    assert out["success"], f"{label} failed:\n{out['stdout']}\n{out['stderr']}"
    return out


def mysql_run(mysql, client_base, env, sql, rows):
    args = client_base + ["--database=test"]
    if rows:
        args += ["--batch", "--raw", "--skip-column-names"]
    args += ["--execute=" + sql]
    return fx_run(mysql, args, env, "")


def failed_mysql(mysql, client_base, env, sql, needle, label):
    out = mysql_run(mysql, client_base, env, sql, False)
    assert not out["success"], f"{label} unexpectedly succeeded"
    assert needle in out["stderr"], f"{label} did not report {needle}: {out['stderr']}"
    return out


def exercise_contract(mysql, client_base, env, key, label):
    table = "contract_" + label
    alias = "alias_" + label
    create = (
        "CREATE TABLE "
        + table
        + " (ordinal BIGINT UNSIGNED NOT NULL PRIMARY KEY) "
        + "ENGINE=YESNO CONNECTION='key="
        + key
        + "'"
    )
    checked(mysql_run(mysql, client_base, env, create, False), label + " create")

    values = "(0), (1), (7), (9), (65536), (18446744073709551614)"
    checked(
        mysql_run(mysql, client_base, env, "INSERT INTO " + table + " VALUES " + values, False),
        label + " boundary insert",
    )
    rows = checked(
        mysql_run(mysql, client_base, env, "SELECT ordinal FROM " + table + " ORDER BY ordinal", True),
        label + " ordered scan",
    )
    assert rows["stdout"] == "0\n1\n7\n9\n65536\n18446744073709551614\n"

    forward = checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "SELECT ordinal FROM " + table + " WHERE ordinal >= 7 AND ordinal < 65536 ORDER BY ordinal",
            True,
        ),
        label + " forward range",
    )
    assert forward["stdout"] == "7\n9\n"
    reverse = checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "SELECT ordinal FROM " + table + " WHERE ordinal <= 65536 AND ordinal > 7 ORDER BY ordinal DESC",
            True,
        ),
        label + " reverse range",
    )
    assert reverse["stdout"] == "65536\n9\n"
    present = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=7", True),
        label + " exact present",
    )
    missing = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=8", True),
        label + " exact missing",
    )
    assert present["stdout"] == "1\n" and missing["stdout"] == "0\n"

    duplicate = mysql_run(mysql, client_base, env, "INSERT INTO " + table + " VALUES (7)", False)
    assert not duplicate["success"], f"{label} accepted a duplicate primary key"
    cardinality = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table, True),
        label + " exact cardinality",
    )
    assert cardinality["stdout"] == "6\n"
    null_insert = mysql_run(mysql, client_base, env, "INSERT INTO " + table + " VALUES (NULL)", False)
    assert not null_insert["success"], f"{label} accepted a NULL ordinal"
    cardinality = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table, True),
        label + " cardinality after refused NULL",
    )
    assert cardinality["stdout"] == "6\n"

    reserved = mysql_run(
        mysql,
        client_base,
        env,
        "INSERT INTO " + table + " VALUES (18446744073709551615)",
        False,
    )
    assert not reserved["success"], f"{label} stored yesno's reserved UINT64_MAX ordinal"
    cardinality = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table, True),
        label + " cardinality after refused boundary",
    )
    assert cardinality["stdout"] == "6\n"

    failed_mysql(
        mysql,
        client_base,
        env,
        "UPDATE " + table + " SET ordinal=8 WHERE ordinal=7",
        "UPDATE is not supported",
        label + " update refusal",
    )
    checked(
        mysql_run(mysql, client_base, env, "DELETE FROM " + table + " WHERE ordinal=9", False),
        label + " delete present",
    )
    checked(
        mysql_run(mysql, client_base, env, "DELETE FROM " + table + " WHERE ordinal=12345", False),
        label + " delete absent",
    )

    alias_create = (
        "CREATE TABLE "
        + alias
        + " (ordinal BIGINT UNSIGNED NOT NULL PRIMARY KEY) "
        + "ENGINE=YESNO CONNECTION='key="
        + key
        + "'"
    )
    checked(mysql_run(mysql, client_base, env, alias_create, False), label + " alias create")
    aliased = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + alias, True),
        label + " alias read",
    )
    assert aliased["stdout"] == "5\n"
    checked(
        mysql_run(mysql, client_base, env, "INSERT INTO " + alias + " VALUES (11)", False),
        label + " alias write",
    )
    visible = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=11", True),
        label + " alias visibility",
    )
    assert visible["stdout"] == "1\n"
    checked(mysql_run(mysql, client_base, env, "DROP TABLE " + alias, False), label + " alias drop")
    cleared = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table, True),
        label + " alias drop clears key",
    )
    assert cleared["stdout"] == "0\n"

    # Transactions, as of 2026-09-19. This assertion used to require the
    # opposite -- a rolled-back INSERT was retained, and the engine advertised
    # HA_NO_TRANSACTIONS -- and it was changed deliberately with the engine's
    # contract, not to make a red test green. Writes are buffered per connection
    # and applied as one yesno batch at commit.
    checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "START TRANSACTION; INSERT INTO " + table + " VALUES (13); ROLLBACK",
            False,
        ),
        label + " rollback probe",
    )
    rolled_back = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=13", True),
        label + " rollback discards the write",
    )
    assert rolled_back["stdout"] == "0\n", "ROLLBACK must discard a buffered INSERT"

    # The mirror case. Without it the assertion above passes against an engine
    # that drops every write, which is the failure a rollback test cannot see
    # on its own.
    checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "START TRANSACTION; INSERT INTO " + table + " VALUES (13); COMMIT",
            False,
        ),
        label + " commit probe",
    )
    committed = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=13", True),
        label + " commit applies the write",
    )
    assert committed["stdout"] == "1\n", "COMMIT must apply a buffered INSERT"

    # Read-your-writes: the buffer also feeds reads, or a transaction cannot see
    # what it just wrote.
    own = checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "START TRANSACTION; INSERT INTO " + table
            + " VALUES (14); SELECT COUNT(*) FROM " + table
            + " WHERE ordinal=14; ROLLBACK",
            True,
        ),
        label + " reads its own writes",
    )
    assert own["stdout"].strip().endswith("1"), "a transaction must see its own buffered INSERT"

    # Savepoints, including the case that needs the parent's verdict to survive:
    # 15 is inserted by the transaction, deleted inside the savepoint, and the
    # rollback must leave it inserted. 16 is written after the savepoint and
    # must vanish.
    checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "START TRANSACTION; INSERT INTO " + table + " VALUES (15);"
            " SAVEPOINT s; DELETE FROM " + table + " WHERE ordinal=15;"
            " INSERT INTO " + table + " VALUES (16);"
            " ROLLBACK TO SAVEPOINT s; COMMIT",
            False,
        ),
        label + " savepoint probe",
    )
    survived = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=15", True),
        label + " savepoint rollback restores the parent's insert",
    )
    assert survived["stdout"] == "1\n", "ROLLBACK TO SAVEPOINT must restore a row the savepoint deleted"
    discarded = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=16", True),
        label + " savepoint rollback discards its own insert",
    )
    assert discarded["stdout"] == "0\n", "ROLLBACK TO SAVEPOINT must discard the savepoint's own INSERT"

    # RELEASE keeps them, which is what stops a fix that discards on every
    # subtransaction end from passing the two assertions above.
    checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "START TRANSACTION; SAVEPOINT r; INSERT INTO " + table
            + " VALUES (17); RELEASE SAVEPOINT r; COMMIT",
            False,
        ),
        label + " savepoint release probe",
    )
    released = checked(
        mysql_run(mysql, client_base, env, "SELECT COUNT(*) FROM " + table + " WHERE ordinal=17", True),
        label + " released savepoint keeps its write",
    )
    assert released["stdout"] == "1\n", "RELEASE SAVEPOINT must keep the savepoint's writes"

    checked(mysql_run(mysql, client_base, env, "DROP TABLE " + table, False), label + " drop")


def exercise_schema_errors(mysql, client_base, env):
    failed_mysql(
        mysql,
        client_base,
        env,
        "CREATE TABLE bad_connection (ordinal BIGINT UNSIGNED NOT NULL PRIMARY KEY) ENGINE=YESNO CONNECTION='key=-1'",
        "CONNECTION must be exactly",
        "invalid connection",
    )
    failed_mysql(
        mysql,
        client_base,
        env,
        "CREATE TABLE bad_signed (ordinal BIGINT NOT NULL PRIMARY KEY) ENGINE=YESNO CONNECTION='key=1'",
        "BIGINT UNSIGNED NOT NULL",
        "signed schema",
    )
    failed_mysql(
        mysql,
        client_base,
        env,
        "CREATE TABLE bad_primary (ordinal BIGINT UNSIGNED NOT NULL) ENGINE=YESNO CONNECTION='key=1'",
        "requires a primary key",
        "missing primary key",
    )
    failed_mysql(
        mysql,
        client_base,
        env,
        "CREATE TABLE bad_extra (ordinal BIGINT UNSIGNED NOT NULL PRIMARY KEY, payload INT) ENGINE=YESNO CONNECTION='key=1'",
        "exactly one BIGINT UNSIGNED column",
        "extra column",
    )


def run_backend(label, backend_args, plugin_preloaded, run_fixture):
    work = fx_temp("mysql-" + label)
    mysql_home = fx_join(work, "prefix")
    datadir = fx_join(work, "data")
    socket_dir = fx_join(work, "socket")
    socket = fx_join(socket_dir, "mysql.sock")
    fx_copy_tree(mysql_source, mysql_home)
    fx_mkdir(socket_dir)

    bin_dir = fx_join(mysql_home, "bin")
    lib_dir = fx_join(mysql_home, "lib")
    private_lib = fx_join(lib_dir, "private")
    plugin_dir = fx_join(lib_dir, "plugin")
    mysqld = fx_join(bin_dir, "mysqld")
    mysql = fx_join(bin_dir, "mysql")
    mysqltest = fx_join(bin_dir, "mysqltest")
    loader = "LD_LIBRARY_PATH=" + private_lib + ":" + lib_dir + ":" + plugin_dir
    env = [loader, "PATH=" + bin_dir]

    checked(
        fx_run(
            mysqld,
            [
                "--no-defaults",
                "--initialize-insecure",
                "--basedir=" + mysql_home,
                "--datadir=" + datadir,
            ],
            env,
            "",
        ),
        label + " mysqld --initialize-insecure",
    )
    server_args = [
        "--no-defaults",
        "--basedir=" + mysql_home,
        "--datadir=" + datadir,
        "--socket=" + socket,
        "--pid-file=" + fx_join(work, "mysqld.pid"),
        "--log-error=" + fx_join(work, "mysqld.log"),
        "--plugin-dir=" + plugin_dir,
        "--skip-networking",
        "--skip-log-bin",
        "--performance-schema=OFF",
        "--innodb-flush-log-at-trx-commit=0",
        "--sync-binlog=0",
    ] + backend_args
    server = fx_start(mysqld, server_args, env)
    client_base = [
        "--no-defaults",
        "--protocol=SOCKET",
        "--socket=" + socket,
        "--user=root",
    ]
    fx_wait_ready(server, mysql, client_base + ["--execute=SELECT 1"], env, 60000)
    assert fx_alive(server)
    checked(fx_run(mysql, client_base + ["--execute=CREATE DATABASE test"], env, ""), label + " create database")

    if not plugin_preloaded:
        checked(
            mysql_run(mysql, client_base, env, "INSTALL PLUGIN yesno SONAME 'ha_yesno.so'", False),
            label + " install plugin",
        )
    engine = checked(
        mysql_run(
            mysql,
            client_base,
            env,
            "SELECT PLUGIN_STATUS FROM information_schema.PLUGINS WHERE PLUGIN_NAME='YESNO'",
            True,
        ),
        label + " plugin identity",
    )
    assert engine["stdout"] == "ACTIVE\n"

    exercise_schema_errors(mysql, client_base, env)
    exercise_contract(mysql, client_base, env, "1000", label)

    checked(mysql_run(mysql, client_base, env, "UNINSTALL PLUGIN yesno", False), label + " uninstall plugin")
    if run_fixture:
        fixture = fx_run(mysqltest, client_base + ["--database=test"], env, fx_read(test_sql))
        checked(fixture, label + " mysqltest")
        expected = fx_read(expected_file)
        assert fixture["stdout"] == expected, (
            f"mysqltest differs\n--- expected ---\n{expected}\n--- actual ---\n{fixture['stdout']}"
        )
    assert fx_alive(server)
    checked(fx_run(mysql, client_base + ["--execute=SHUTDOWN"], env, ""), label + " shutdown")
    checked(fx_close(server, 10000), label + " mysqld exit")


run_backend("embedded", [], False, True)
flight_http = fx_flight_start([], [])
flight_grpc = "grpc://" + flight_http[7:]
run_backend(
    "flight",
    [
        "--plugin-load-add=ha_yesno.so",
        "--yesno-backend=flight",
        "--yesno-flight-endpoint=" + flight_grpc,
    ],
    True,
    False,
)
fx_flight_stop()
