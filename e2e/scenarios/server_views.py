# Packed views through the daemon's actual Flight socket.
#
# The physical ordinals are constructed here, independently of yesno's view
# implementation. Python sets are the oracle for every logical answer. The
# restart proves the descriptor is request state and the packed bits are normal
# durable key data; there is deliberately no server-side view catalogue.

parts = [
    {1, 8, 55, 144},
    {2, 8, 89, 144},
    {3, 8, 144, 233},
]
union = parts[0] | parts[1] | parts[2]
intersection = parts[0] & parts[1] & parts[2]
parity = parts[0] ^ parts[1] ^ parts[2]

cfg = srv_config("view-node", shards=2, interval_secs=1)
srv_control_admin(cfg, "all")
srv = srv_launch(cfg)
f = srv_flight(srv)

interleaved = vw_interleaved(3)
blocked = vw_blocked(3, 65536)
keys = []
ordinals = []
for i, part in enumerate(parts):
    for x in part:
        keys.append(90)
        ordinals.append(x * 3 + i)
        keys.append(91)
        ordinals.append(i * 65536 + x)
assert flight_put(f, keys, ordinals) == len(keys)

for key, view in [(90, interleaved), (91, blocked)]:
    for i, want in enumerate(parts):
        result = flight_view_select(f, key, view, i)
        assert result["total_records"] == len(want), result
        assert result["rows"] == sorted(want), (key, i, result)

    # The fold operator is named for the Boolean operation being folded, which
    # is what the wire and the query language now spell. It used to be
    # "any" / "all" / "parity".
    for op, want in [
        ("or", union),
        ("and", intersection),
        ("xor", parity),
    ]:
        result = flight_view_fold(f, key, view, op)
        assert result["total_records"] == len(want), result
        assert result["rows"] == sorted(want), (key, op, result)

assert srv_checkpoint(srv) > 0
flight_stop(f)
stopped = srv_stop(srv)
assert stopped["clean"] is True, stopped
assert stopped["lock_released"] is True, stopped

# Reuse the same caller-owned descriptors after restart. If the server had
# smuggled a descriptor into process memory, this is where the query would fail.
srv = srv_start("view-node", shards=2, interval_secs=1)
f = srv_flight(srv)
for key, view in [(90, interleaved), (91, blocked)]:
    result = flight_view_select(f, key, view, 1)
    assert result["total_records"] == len(parts[1]), result
    assert result["rows"] == sorted(parts[1]), result
    result = flight_view_fold(f, key, view, "and")
    assert result["rows"] == sorted(intersection), result

flight_stop(f)
stopped = srv_stop(srv)
assert stopped["clean"] is True, stopped

print("server_views: both layouts select and fold through Flight across restart")
