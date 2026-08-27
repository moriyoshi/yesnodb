# Reclaim archive objects outside a retention window, and prove the boundary.
#
# Reclamation is the only thing in this system that deletes durable recovery
# data, so this asserts both halves: a recovery point inside the window still
# restores afterwards, and one the sweep removed fails loudly rather than
# rounding forward to a later base.
#
# Two base generations are needed, and the only way to get a second one is the
# path production takes: force the leader's hard WAL cap past the sidecar's
# durable cursor so a reconnect must re-root rather than bridge the gap.

cfg = srv_config("leader", shards=1, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_archive_access(cfg)
srv_control_admin(cfg, "all")
srv_archive_retention(cfg, 65536)
leader = srv_launch(cfg)
flight = srv_flight(leader)

assert flight_put(flight, [1], [10]) == 1
archive = srv_archive_start(leader, "objects", "archive-work")
assert srv_checkpoint(leader) > 0
first = srv_archive_wait(archive, "base_generation", 1, 30000)
assert first["base_files"] > 0, first


def window():
    windows = srv_restore_windows("objects")
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


assert flight_put(flight, [2], [20]) == 1
old = settled(window()["end_version"] - 1)
t_old = old["end_time"]

# Force the retention gap. The sidecar is down, so the leader reclaims past the
# cursor it last acknowledged, and the reconnect cannot bridge it.
srv_archive_stop(archive)
keys = []
ords = []
for i in range(12000):
    keys.append(1000 + i)
    ords.append(1000000 + i * 3)
assert flight_put(flight, keys, ords) == len(keys)
assert srv_checkpoint(leader) > 0

archive = srv_archive_start(leader, "objects", "archive-work")
rebased = srv_archive_wait(archive, "base_generation", first["base_generation"] + 1, 30000)
assert rebased["base_files"] > 0, rebased
# The floor has to be read *before* the commit. A fresh base already answers
# `settled` on its own -- its window ends at its own checkpoint, with the
# checkpoint-time bound standing in for an end time -- so passing 0 here would
# return the base's window and target a point before the commit below.
base_end = window()["end_version"]
assert flight_put(flight, [3], [30]) == 1
new = settled(base_end)
assert new["base_generation"] > first["base_generation"], (first, new)

srv_archive_stop(archive)
flight_stop(flight)
assert srv_stop(leader)["clean"] is True

assert len(srv_restore_windows("objects")) == 2, srv_restore_windows("objects")

# A wide window with room for two roots must delete nothing. This is the failure
# that would destroy an archive, so it is asserted before the one that deletes.
untouched = srv_archive_gc("objects", 86400, 2)
assert untouched["deleted"] == 0, untouched
assert untouched["retained_bases"] == 2, untouched
# Every key the real sidecar writes must be one this build can classify.
# An unrecognized key is kept, so a regression here shows up as an archive that
# silently stops reclaiming rather than as a failure.
assert untouched["unrecognized"] == 0, untouched
assert len(srv_restore_windows("objects")) == 2, srv_restore_windows("objects")

# Diagnostics kept from the empty-window investigation, which is **closed**.
#
# This scenario used to fail about one run in five inside the full suite and
# pass standalone. Root cause, 2026-09-10: the sidecar publishes a WAL object,
# then advances the cursor, then publishes state -- and a window computed
# `end_version` from the objects it could *list*, while `stage_wal` walks only
# to the durable tip. Stopping the sidecar inside that gap advertised a version
# the restore would not walk to, so it staged nothing and reported success with
# only the base. Fixed in `restore.rs` by not counting an object past the
# durable tip, pinned by `a_window_does_not_count_wal_beyond_the_durable_tip`.
#
# The prints stay: they are what settled it, they cost a passing run nothing,
# and the next window/tip disagreement will want exactly these two values. A
# passing run prints nothing extra unless --show-output; a failing one always
# shows this. **This comment described the flake in the present tense until
# 2026-09-14**, three days after the fix, and a TODO sweep duly filed a backlog
# entry for a bug that no longer existed.
print("commit_window=" + str(new))
print("windows_before_sweep=" + str(srv_restore_windows("objects")))

# Now a window of one second, declining to hold a second root open.
swept = srv_archive_gc("objects", 1, 1)
print("swept=" + str(swept))
print("windows_after_sweep=" + str(srv_restore_windows("objects")))
assert swept["deleted"] > 0, "reclamation removed nothing from a superseded base"
assert swept["retained_bases"] == 1, swept
assert swept["unrecognized"] == 0, swept
assert len(srv_restore_windows("objects")) == 1, srv_restore_windows("objects")

# The retained window still restores, through the shipped restore rather than
# through the window report that just claimed it.
kept = srv_restore_at("objects", "restore-kept", new["end_time"], 1, "publish")
restored = db_open("restore-kept", shards=1)
snapshot = db_snapshot(restored)
# Report what the restore actually holds, not merely that key 3 is absent.
# `[] == [30]` alone cannot distinguish "the window was reclaimed out from under
# the report" from "the restore targeted a point before the commit".
print("restored=" + str(kept))
print("key2=" + str(snap_load(snapshot, 2)) + " key3=" + str(snap_load(snapshot, 3)))
# `recovered_time: None` with `recovered` at the base's own checkpoint means
# the increment after the base was never replayed -- which is a different fault
# from the base itself being short, and the two are indistinguishable from `[]`.
assert snap_load(snapshot, 3) == [30], (kept, new)
snap_release(snapshot)
db_close(restored)

# And the point reclamation removed fails, saying so.
gone = False
try:
    srv_restore_at("objects", "restore-reclaimed", t_old, 1, "publish")
except RuntimeError as e:
    gone = True
    assert "no base checkpoint at or below" in str(e), str(e)
assert gone, "a reclaimed target must fail, not round forward to a later base"

# Reclamation is idempotent: a second pass over the swept archive finds nothing.
again = srv_archive_gc("objects", 1, 1)
assert again["deleted"] == 0, again
assert again["unrecognized"] == 0, again
