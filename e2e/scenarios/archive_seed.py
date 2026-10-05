# A new follower starts from a durable object archive, then receives writes
# committed after the seed through the ordinary live replication path.

cfg = srv_config("leader", shards=2, interval_secs=3600)
srv_control_admin(cfg, "all")
srv_archive_access(cfg)
srv_replication(cfg)
leader = srv_launch(cfg)
leader_flight = srv_flight(leader)

assert flight_put(leader_flight, [1], [10]) == 1
archive = srv_archive_start(leader, "objects", "archive-work")
assert srv_checkpoint(leader) > 0
base = srv_archive_wait(archive, "base_generation", 1, 30000)
assert base["base_files"] > 0, base

assert flight_put(leader_flight, [2], [20]) == 1
srv_archive_wait(archive, "cursor_total", base["cursor_total"] + 1, 30000)

follower_cfg = srv_config("new-follower", shards=2)
srv_follows(follower_cfg, leader)
srv_serve_reads(follower_cfg)
assert srv_seed_follower(follower_cfg, leader, "objects") == "Seeded"
assert srv_seed_follower(follower_cfg, leader, "objects") == "Existing"
srv_archive_stop(archive)

assert flight_put(leader_flight, [3], [30]) == 1
follower = srv_launch(follower_cfg)
follower_flight = srv_flight(follower)
srv_follower_wait(follower, "records", 1, 30000)
for _ in range(100):
    if flight_info(follower_flight, 3)["total_records"] == 1:
        break
    srv_follower_wait(follower, "passes", srv_follower_state(follower)["passes"] + 1, 10000)

assert flight_get(follower_flight, 1) == [10]
assert flight_get(follower_flight, 2) == [20]
assert flight_get(follower_flight, 3) == [30]
assert srv_follower_state(follower)["halted"] is False
assert srv_follower_state(follower)["rebootstraps"] == 0

flight_stop(follower_flight)
flight_stop(leader_flight)
assert srv_stop(follower)["clean"] is True
assert srv_stop(leader)["clean"] is True
