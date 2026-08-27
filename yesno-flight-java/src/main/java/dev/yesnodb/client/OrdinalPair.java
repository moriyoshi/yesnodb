package dev.yesnodb.client;

/** One {@code (key, ordinal)} mutation; both components carry raw unsigned 64-bit bits. */
public record OrdinalPair(long key, long ordinal) {}
