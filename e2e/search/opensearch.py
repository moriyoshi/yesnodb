# Install the version-locked OpenSearch plugin into a disposable engine and
# compare its document matches with Python's set oracle.

db = db_open("search-opensearch")
db_insert_many(db, 42, [1, 3, 5])
db_insert_many(db, 91, [3, 4])
flight = flight_serve(db)
location = flight_location(flight)

assert search_engine_start(location, "opensearch") == "opensearch"
corpus = [0, 1, 2, 3, 4, 5, 6]
assert search_index(corpus) == len(corpus)
assert search_query(42) == sorted({1, 3, 5})
assert search_query(91) == sorted({3, 4})
assert search_engine_stop() is True

flight_stop(flight)
db_close(db)
