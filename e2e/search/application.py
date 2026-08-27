# Exercise the application-side Java adapters against the ordinary Flight
# service verb. This lives outside e2e/scenarios because it needs JDK 21, but
# it runs through exactly the same Monty runner and shared verb surface.

db = db_open("search-application")
db_insert_many(db, 42, [1, 3, 5])
flight = flight_serve(db)
location = flight_location(flight)

assert search_application(location) is True

flight_stop(flight)
db_close(db)
