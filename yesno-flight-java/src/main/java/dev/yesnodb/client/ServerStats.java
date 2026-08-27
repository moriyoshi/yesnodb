package dev.yesnodb.client;

/** Server space and reader counters; every component carries raw unsigned 64-bit bits. */
public record ServerStats(
    long allocatedBytes, long deferredBytes, long walBytes, long liveReaders, long shards) {}
