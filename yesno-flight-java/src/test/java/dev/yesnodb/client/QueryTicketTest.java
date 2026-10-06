package dev.yesnodb.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertSame;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.HexFormat;
import org.junit.jupiter.api.Test;

class QueryTicketTest {
  /**
   * A fixed byte vector for the header, mirrored verbatim from {@code
   * the_cross_implementation_ticket_header_is_stable} in the Rust crate: version(8) key(8)
   * prefix_lo(8) prefix_hi(8) expr_hash(8) wire(8).
   *
   * <p>Written as a literal on purpose. The header widened from 40 to 48 bytes on 2026-09-30 and
   * this module did not notice, because the only server its tests talk to is a fake that minted its
   * ticket from {@code QueryTicket.HEADER_LENGTH}. A test that derives its fixture from the
   * constant it is checking cannot fail; this one can, and a Java gate with no live daemon has
   * nothing else that would.
   */
  private static final String CANONICAL_HEADER =
      "09000000000000002a0000000000000001000000000000000000100000000000"
          + "07000000000000000000000000000000";

  @Test
  void theCrossImplementationTicketHeaderIsStable() {
    byte[] encoded = HexFormat.of().parseHex(CANONICAL_HEADER);
    assertEquals(QueryTicket.HEADER_LENGTH, encoded.length);

    QueryTicket ticket = QueryTicket.decode(encoded);
    assertEquals(9, ticket.version());
    assertEquals(42, ticket.key());
    assertEquals(1, ticket.prefixLo());
    assertEquals(1L << 20, ticket.prefixHi());
    assertEquals(7, ticket.expressionHash());
    assertSame(SetWire.ORDINALS, ticket.wire());
    assertEquals(java.util.Optional.empty(), ticket.expression());
  }

  @Test
  void fixedHeaderAndExpressionTicketsDecode() {
    SetExpression expression = SetExpression.and(SetExpression.key(42), SetExpression.range(0, 10));
    ByteBuffer bytes =
        ByteBuffer.allocate(QueryTicket.HEADER_LENGTH + expression.encode().length)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putLong(9)
            .putLong(42)
            .putLong(1)
            .putLong(1L << 48)
            .putLong(7)
            .putLong(SetWire.ORDINALS.code())
            .put(expression.encode());

    QueryTicket ticket = QueryTicket.decode(bytes.array());
    assertEquals(9, ticket.version());
    assertEquals(42, ticket.key());
    assertEquals(1, ticket.prefixLo());
    assertEquals(1L << 48, ticket.prefixHi());
    assertEquals(7, ticket.expressionHash());
    assertEquals(expression, ticket.expression().orElseThrow());
  }

  /**
   * The representation lives in the header's sixth word, so its position is checked and not merely
   * its presence: a vector for the default alone would still pass if the field moved, because its
   * bytes are zero.
   */
  @Test
  void theRequestedRepresentationIsTheSixthHeaderWord() {
    byte[] containers =
        HexFormat.of()
            .parseHex(
                "09000000000000002a0000000000000001000000000000000000100000000000"
                    + "07000000000000000100000000000000");
    assertSame(SetWire.CONTAINERS, QueryTicket.decode(containers).wire());

    for (SetWire wire : SetWire.values()) {
      byte[] encoded =
          ByteBuffer.allocate(QueryTicket.HEADER_LENGTH)
              .order(ByteOrder.LITTLE_ENDIAN)
              .putLong(1)
              .putLong(2)
              .putLong(0)
              .putLong(1L << 48)
              .putLong(0)
              .putLong(wire.code())
              .array();
      assertSame(wire, QueryTicket.decode(encoded).wire());
    }
  }

  /**
   * An unrecognised representation rejects the ticket rather than defaulting. Reading a stream in a
   * representation the server did not promise is corruption, and indistinguishable from a server
   * that understood.
   */
  @Test
  void unknownRepresentationIsRejected() {
    byte[] encoded =
        ByteBuffer.allocate(QueryTicket.HEADER_LENGTH)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putLong(1)
            .putLong(2)
            .putLong(0)
            .putLong(1L << 48)
            .putLong(0)
            .putLong(200)
            .array();
    assertThrows(IllegalArgumentException.class, () -> QueryTicket.decode(encoded));
  }

  @Test
  void shortInvertedAndUnparseableTicketsAreRejected() {
    assertThrows(
        IllegalArgumentException.class,
        () -> QueryTicket.decode(new byte[QueryTicket.HEADER_LENGTH - 1]));

    ByteBuffer inverted =
        ByteBuffer.allocate(QueryTicket.HEADER_LENGTH)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putLong(1)
            .putLong(1)
            .putLong(10)
            .putLong(5)
            .putLong(0)
            .putLong(0);
    assertThrows(IllegalArgumentException.class, () -> QueryTicket.decode(inverted.array()));

    byte[] trailing = new byte[QueryTicket.HEADER_LENGTH + 1];
    assertThrows(IllegalArgumentException.class, () -> QueryTicket.decode(trailing));
  }
}
