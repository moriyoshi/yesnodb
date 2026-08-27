# Recover an archived database to a wall-clock instant.
#
# Three groups of commits are archived in order. Restoring to the instant that
# ends the first group must produce exactly that group: the second and third are
# later in time, so a correct resolution excludes them and an off-by-one on the
# comparison includes one of them.
#
# The target times come from the shipped inspect report, never from clock_ns().
# The harness clock is not the server's commit clock, and asserting against it
# would be a test of two clocks agreeing rather than of the resolution rule.

SHARDS = 2

cfg = srv_config("leader", shards=SHARDS, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_archive_access(cfg)
srv_control_admin(cfg, "all")
leader = srv_launch(cfg)
flight = srv_flight(leader)

# A base only becomes a recovery root once a checkpoint has been observed, so
# the first group must be committed after the archive is live and rooted.
assert flight_put(flight, [1], [10]) == 1
archive = srv_archive_start(leader, "objects", "archive-work")
assert srv_checkpoint(leader) > 0
base = srv_archive_wait(archive, "base_generation", 1, 30000)
assert base["base_files"] > 0, base


def window():
    windows = srv_restore_windows("objects")
    assert len(windows) > 0, windows
    active = [w for w in windows if w["active"] == 1]
    assert len(active) == 1, windows
    return active[0]


def settled(previous_end):
    # Wait until the commit just made is *restorable by wall clock*, which is
    # two conditions, not one.
    #
    # The archive cursor alone is not enough: it advances whenever the sidecar
    # takes any frame, including the tail of an earlier group.
    # A higher end version alone is not enough either. Objects are normalized
    # to individual frames, so a commit's data frames can be archived before its
    # commit marker — and the marker is the only frame carrying the time.
    # A version present with no stamp is a commit that is not yet answerable.
    #
    # The cursor is pacing only, never the assertion, so its wait timing out
    # is normal rather than fatal. Once the sidecar has caught up completely no
    # further acknowledgement is coming at all, and a window read taken just
    # before that moment would otherwise wait forever for an advance that has
    # already happened. Only the window decides.
    #
    # The cursor wait is the *fast* path and is kept long enough to be one
    # wait rather than many: a window read lists and decodes the whole archive,
    # so polling it is far more expensive than blocking on an acknowledgement.
    # Each attempt blocks in the host with a real deadline and the loop is
    # bounded, so this waits without spinning.
    for _ in range(20):
        current = window()
        if current["end_version"] > previous_end and current["end_time"] is not None:
            return current
        cursor = srv_archive_wait(archive, "cursor_total", 0, 5000)["cursor_total"]
        try:
            srv_archive_wait(archive, "cursor_total", cursor + 1, 5000)
        except ValueError:
            pass
    raise RuntimeError("archive never made a stamped commit past version %d" % previous_end)


start = window()["end_version"]

# --- group A: an earlier commit, then the single commit that ends the group -
assert flight_put(flight, [100, 101, 102, 103, 104], [1, 2, 3, 4, 5]) == 5
early_a = settled(start)
assert flight_put(flight, [110], [11]) == 1
after_a = settled(early_a["end_version"])

assert after_a["fully_stamped"] == 1, after_a
t_a = after_a["end_time"]
v_a = after_a["end_version"]
assert t_a > early_a["end_time"], (early_a, after_a)

# --- group B ----------------------------------------------------------------
assert flight_put(flight, [200, 201], [20, 21]) == 2
after_b = settled(v_a)
t_b = after_b["end_time"]
assert t_b > t_a, (after_a, after_b)

# --- group C ----------------------------------------------------------------
assert flight_put(flight, [300], [30]) == 1
after_c = settled(after_b["end_version"])
assert after_c["end_time"] > t_b, (after_b, after_c)


# --- inclusive: the commit stamped exactly at t_a is kept -------------------
inclusive = srv_restore_at("objects", "restore-a", t_a, 1, "publish")
assert inclusive["recovered"] == v_a, inclusive
assert inclusive["recovered_time"] == t_a, inclusive
assert inclusive["shards"] == SHARDS, inclusive
assert inclusive["published"] == 1, inclusive

restored = db_open("restore-a", shards=SHARDS)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 1) == [10]
for key in range(100, 105):
    assert snap_load(snapshot, key) == [key - 99], key
assert snap_load(snapshot, 110) == [11], "the boundary commit must be included"
assert snap_load(snapshot, 200) == [], "group B is later in time"
assert snap_load(snapshot, 201) == []
assert snap_load(snapshot, 300) == [], "group C is later in time"
snap_release(snapshot)
db_close(restored)

# --- exclusive: the same instant now cuts the boundary commit away ----------
exclusive = srv_restore_at("objects", "restore-a-excl", t_a, 0, "publish")
assert exclusive["recovered"] < v_a, exclusive

restored = db_open("restore-a-excl", shards=SHARDS)
snapshot = db_snapshot(restored)
for key in range(100, 105):
    assert snap_load(snapshot, key) == [key - 99], key
assert snap_load(snapshot, 110) == [], "an exclusive target must drop its own instant"
snap_release(snapshot)
db_close(restored)

# --- pause publishes nothing ------------------------------------------------
paused = srv_restore_at("objects", "restore-paused", t_b, 1, "pause")
assert paused["published"] == 0, paused
assert paused["recovered"] > v_a, paused
assert paused["directory"] != "restore-paused", paused

# --- promote raises the term, so the copy is a new timeline -----------------
promoted = srv_restore_at("objects", "restore-promoted", t_b, 1, "promote")
assert promoted["published"] == 1, promoted
assert promoted["term"] == after_b["term"] + 1, (promoted, after_b)

restored = db_open("restore-promoted", shards=SHARDS)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 200) == [20], "group B is at or before t_b"
assert snap_load(snapshot, 300) == [], "group C is later"
snap_release(snapshot)
db_close(restored)

# --- a target below the archive's first recovery point is refused ----------
refused = False
try:
    srv_restore_at("objects", "restore-too-early", 1, 1, "publish")
except RuntimeError as e:
    refused = True
    assert "no base checkpoint at or below" in str(e), str(e)
assert refused, "a target before every base must fail rather than round forward"

# --- reclamation keeps the window restorable -------------------------------
#
# One base and one window here, so a pass has nothing it may delete: the active
# root is never a candidate and the window covers the whole history. A pass that
# deletes anything at all under these conditions is the failure worth catching,
# because it is the one that destroys an archive.
before = srv_restore_windows("objects")
swept = srv_archive_gc("objects", 86400, 2)
assert swept["deleted"] == 0, swept
assert swept["retained_bases"] == len(before), (swept, before)
assert swept["unrecognized"] == 0, "the archive holds objects this build cannot classify"

after = srv_restore_windows("objects")
assert after == before, (before, after)

# The recovery point asserted above must still be reachable, through the shipped
# restore rather than through the window report that just claimed it.
again = srv_restore_at("objects", "restore-after-gc", t_a, 1, "publish")
assert again["recovered"] == v_a, again
restored = db_open("restore-after-gc", shards=SHARDS)
snapshot = db_snapshot(restored)
assert snap_load(snapshot, 110) == [11], "reclamation broke a retained recovery point"
assert snap_load(snapshot, 300) == []
snap_release(snapshot)
db_close(restored)

srv_archive_stop(archive)
flight_stop(flight)
assert srv_stop(leader)["clean"] is True
