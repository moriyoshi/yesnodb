package dev.yesnodb.client.live;

import dev.yesnodb.client.Mutation;
import dev.yesnodb.client.OrdinalPair;
import dev.yesnodb.client.QueryInfo;
import dev.yesnodb.client.ServerStats;
import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.YesnoClient;
import java.io.PrintStream;
import java.net.URI;
import java.util.ArrayList;
import java.util.List;
import java.util.StringJoiner;
import org.apache.arrow.flight.FlightRuntimeException;
import org.apache.arrow.flight.FlightStream;
import org.apache.arrow.flight.Location;
import org.roaringbitmap.RoaringBitmap;

/**
 * Drives the shipped Java client against a live {@code yesnod} and prints what it observed.
 *
 * <p>This is a <b>driver</b>, not a test. It asserts nothing about the values it reads; it reports
 * them on standard output and the Monty scenario that started the daemon checks them against
 * Python's own {@code set}. The one thing it does decide locally is the <i>name</i> of a refusal,
 * because a Flight status code is not an observation the harness can make from outside.
 *
 * <p>It exists because every other live check of this client ran against a fake server inside its
 * own unit tests -- a fake that minted tickets from the client's own constants, so the module
 * stayed green through a wire-format change that broke the Go and Python clients outright.
 *
 * <p>Deliberately a {@code main} rather than a JUnit test: it runs from a classpath staged into the
 * E2E image at build time, where neither Gradle nor a Maven Central round trip is available.
 */
public final class LiveDaemonCheck {
  private static final long KEY_A = 7001;
  private static final long KEY_B = 7002;
  private static final long KEY_C = 7003;
  private static final long KEY_D = 7004;

  private LiveDaemonCheck() {}

  /**
   * Runs the whole sequence against the daemon at {@code args[0]}.
   *
   * @param args exactly one {@code grpc+tcp://HOST:PORT} location
   * @throws Exception if the daemon refuses a call the check expects to succeed
   */
  public static void main(String[] args) throws Exception {
    if (args.length != 1) {
      System.err.println("usage: LiveDaemonCheck grpc+tcp://HOST:PORT");
      System.exit(2);
      return;
    }
    URI endpoint = URI.create(args[0]);
    Location location = Location.forGrpcInsecure(endpoint.getHost(), endpoint.getPort());
    try (YesnoClient client = YesnoClient.connect(location)) {
      run(client, System.out);
    }
    System.out.flush();
  }

  private static void run(YesnoClient client, PrintStream out) throws Exception {
    // ---- the capability handshake a caller is told to make before writing
    ServerStats initial = client.stats();
    emitBool(out, "supports_mixed_put", initial.supports(ServerStats.FEATURE_MIXED_PUT));
    emitBool(
        out,
        "supports_write_transactions",
        initial.supports(ServerStats.FEATURE_WRITE_TRANSACTIONS));
    emitInt(out, "shards", initial.shards());

    // ---- DoPut, streamed in the client's own batches
    List<OrdinalPair> alpha = new ArrayList<>();
    for (long ordinal : new long[] {1, 3, 5, 7, 9}) {
      alpha.add(new OrdinalPair(KEY_A, ordinal));
    }
    emitInt(out, "insert_rows", client.insert(alpha));

    // ---- DoPut as one batch and one commit, which is the read-your-writes primitive
    YesnoClient.IngestAck acked =
        client.insertBatchAcked(List.of(new OrdinalPair(KEY_B, 2), new OrdinalPair(KEY_B, 4)));
    emitInt(out, "acked_rows", acked.rows());

    // ---- ListFlights
    emitLongs(out, "keys", client.keys());

    // ---- GetFlightInfo: planning, with no ordinals fetched
    emitInt(out, "cardinality_a", client.cardinality(KEY_A));
    emitInt(
        out,
        "cardinality_a_or_b",
        client.queryCardinality(SetExpression.or(SetExpression.key(KEY_A), SetExpression.key(KEY_B))));

    // ---- the ticket round trip, which is what a fake server cannot check.
    // `ticket_bytes` is the one number the cross-implementation hex vector also
    // pins; the refetch below is the half it cannot reach, because only a real
    // server decides whether it accepts what it minted.
    QueryInfo planned = client.prepareKey(KEY_A);
    emitInt(out, "ticket_bytes", planned.ticketBytes().length);
    emitBool(out, "planned_version_is_acked_version", planned.version() == acked.version());
    emitInt(out, "refetched_rows", drain(client.fetchTicket(planned.ticketBytes())));

    // ---- DoGet through the documented helpers, decoded as roaring
    emitInts(out, "ordinals_a", client.getRoaringBitmap(KEY_A));
    emitInts(
        out,
        "ordinals_a_without_3_5",
        client.queryRoaringBitmap(
            SetExpression.andNot(SetExpression.key(KEY_A), SetExpression.literal(3, 5))));

    // ---- mixed operations in one commit, in the order given
    YesnoClient.IngestAck applied =
        client.apply(
            List.of(
                Mutation.insert(KEY_B, 6),
                Mutation.remove(KEY_B, 2),
                Mutation.insertRange(KEY_B, 100, 103)));
    emitInt(out, "apply_rows", applied.rows());
    emitBool(out, "apply_advanced_version", Long.compareUnsigned(applied.version(), acked.version()) > 0);
    emitInts(out, "ordinals_b", client.getRoaringBitmap(KEY_B));

    // ---- a write transaction, and the invisibility of what it has staged
    long committed = client.beginWrite();
    emitInt(
        out,
        "staged_rows",
        client.stage(
            committed, List.of(Mutation.insert(KEY_C, 11), Mutation.insert(KEY_C, 13))));
    emitInts(out, "ordinals_c_staged", client.getRoaringBitmap(KEY_C));
    long commitVersion = client.commitWrite(committed);
    emitBool(
        out,
        "commit_advanced_version",
        Long.compareUnsigned(commitVersion, applied.version()) > 0);
    emitInts(out, "ordinals_c_committed", client.getRoaringBitmap(KEY_C));

    // ---- and one that is thrown away
    long aborted = client.beginWrite();
    client.stage(aborted, List.of(Mutation.insert(KEY_D, 21)));
    client.abortWrite(aborted);
    emitInts(out, "ordinals_d_aborted", client.getRoaringBitmap(KEY_D));

    // ---- the refusals. A status code is the half of the contract that a
    // literal wire vector cannot express, and the half that changes silently.
    emitString(
        out,
        "short_ticket_status",
        statusOf(() -> drain(client.fetchTicket(new byte[8]))));
    emitString(out, "recommit_status", statusOf(() -> client.commitWrite(committed)));
    emitString(out, "abort_after_commit_status", statusOf(() -> client.abortWrite(committed)));
  }

  // There is deliberately no reader-count observation here. A `stats` call
  // cannot see the teardown of the streams this same client just closed: the
  // server unwinds those tasks after it has answered, so any number read from
  // inside the session races its own cleanup. The scenario asserts
  // `srv_stop`'s `readers_left` instead, which is taken after the daemon has
  // drained and is the only place the question has a settled answer. The first
  // live run reported three live readers here with nothing actually leaked.

  /**
   * Reads a stream to exhaustion and returns the rows it carried.
   *
   * <p>Draining is not incidental: a Flight server reports a bad ticket when the client pulls, so
   * a refusal only arrives here.
   *
   * <p>The suppression is scoped to this method on purpose. `-Xlint:try` objects that {@code
   * FlightStream.close()} declares {@code InterruptedException}, which is exactly what we want it
   * to do -- the check has nothing useful to do with an interrupt but abort -- and silencing it on
   * a larger method would hide the next resource that deserves the warning.
   */
  @SuppressWarnings("try")
  private static long drain(FlightStream stream) throws Exception {
    long rows = 0;
    try (FlightStream owned = stream) {
      while (owned.next()) {
        rows += owned.getRoot().getRowCount();
      }
    }
    return rows;
  }

  /** A call whose only interesting result is whether, and how, the server refused it. */
  private interface Call {
    void run() throws Exception;
  }

  private static String statusOf(Call call) throws Exception {
    try {
      call.run();
    } catch (FlightRuntimeException refusal) {
      return refusal.status().code().name();
    }
    return "OK";
  }

  private static void emitInt(PrintStream out, String name, long value) {
    out.println("yesno-live int " + name + " " + Long.toUnsignedString(value));
  }

  private static void emitBool(PrintStream out, String name, boolean value) {
    out.println("yesno-live bool " + name + " " + value);
  }

  private static void emitString(PrintStream out, String name, String value) {
    out.println("yesno-live str " + name + " " + value);
  }

  private static void emitLongs(PrintStream out, String name, List<Long> values) {
    StringJoiner joined = new StringJoiner(",");
    for (long value : values) {
      joined.add(Long.toUnsignedString(value));
    }
    out.println("yesno-live list " + name + " " + joined);
  }

  private static void emitInts(PrintStream out, String name, RoaringBitmap bitmap) {
    StringJoiner joined = new StringJoiner(",");
    for (int value : bitmap.toArray()) {
      joined.add(Long.toUnsignedString(Integer.toUnsignedLong(value)));
    }
    out.println("yesno-live list " + name + " " + joined);
  }
}
