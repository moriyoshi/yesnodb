# An object that really is gone, against an archive a real sidecar wrote.
#
# `missing` compares what the retained manifests name against what the store
# holds, and **both sides are derived by code**: the uploader writes a slab to
# `db/<uuid>/slabs/<sha256 of the contents>`, and the planner recomputes that
# key from the manifest's extents. Nothing forces those two derivations to
# agree. If they ever drift, the check reports nothing for ever -- and every
# assertion already in the suite still passes, because `base_dedup.py` asserts
# `missing == 0` on a whole archive, which is exactly what a check that can
# never fire also reports.
#
# The unit tests cover the set difference over hand-built and fixture
# archives, so they share the planner's derivation and cannot catch a drift
# either. Only taking an object away from an archive the shipped uploader
# wrote can. This file does that, and then follows it to the end of the
# chain: the restore the gauge exists to pre-empt.

cfg = srv_config("leader", shards=1, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_archive_access(cfg)
srv_control_admin(cfg, "all")
srv_archive_retention(cfg, 65536)
leader = srv_launch(cfg)
flight = srv_flight(leader)
assert flight_put(flight, [1, 1, 1], [10, 20, 50]) == 3

archive = srv_archive_start(leader, "objects", "archive-work")
version = srv_checkpoint(leader)
assert version > 0
base = srv_archive_wait(archive, "base_generation", 1, 300000)
assert base["slab_objects"] > 0, base
srv_archive_stop(archive)
flight_stop(flight)
assert srv_stop(leader)["clean"] is True

# Whole first. Without this the assertions below would also hold on an archive
# that was already broken, which would make the test about nothing.
whole = srv_archive_gc("objects", 0, 1)
assert whole["retained_bases"] == 1, whole
assert whole["missing"] == 0, whole
assert whole["damaged_bases"] == 0, whole
assert whole["unrecognized"] == 0, whole

# One object, removed the way a lifecycle rule removes one: no manifest is
# rewritten and no state is touched, so every other signal still looks healthy.
lost = srv_archive_lose_slab("objects")
print("lost " + lost)

holed = srv_archive_gc("objects", 0, 1)
assert holed["retained_bases"] == 1, holed
assert holed["missing"] == 1, holed
# The same damage in the unit an operator acts in. One base here, so the two
# numbers agree; they come apart once generations share pieces.
assert holed["damaged_bases"] == 1, holed
assert holed["unrecognized"] == 0, (holed, "a hole is not an unclassified key")

# The end of the chain. A key is a hash, so the message has to name the base
# file that wanted it and point at where the rest of the damage is listed --
# a restore stops at the first absent object and so reports only one.
message = srv_restore_error("objects", "restore-holed", version)
print(message)
assert lost in message, message
assert "archive-plan" in message, message
