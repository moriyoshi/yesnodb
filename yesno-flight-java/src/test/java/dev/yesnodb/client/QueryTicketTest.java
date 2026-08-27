package dev.yesnodb.client;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import org.junit.jupiter.api.Test;

class QueryTicketTest {
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
            .put(expression.encode());

    QueryTicket ticket = QueryTicket.decode(bytes.array());
    assertEquals(9, ticket.version());
    assertEquals(42, ticket.key());
    assertEquals(1, ticket.prefixLo());
    assertEquals(1L << 48, ticket.prefixHi());
    assertEquals(7, ticket.expressionHash());
    assertEquals(expression, ticket.expression().orElseThrow());
  }

  @Test
  void shortInvertedAndUnparseableTicketsAreRejected() {
    assertThrows(IllegalArgumentException.class, () -> QueryTicket.decode(new byte[39]));

    ByteBuffer inverted =
        ByteBuffer.allocate(40)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putLong(1)
            .putLong(1)
            .putLong(10)
            .putLong(5)
            .putLong(0);
    assertThrows(IllegalArgumentException.class, () -> QueryTicket.decode(inverted.array()));

    byte[] trailing = new byte[41];
    assertThrows(IllegalArgumentException.class, () -> QueryTicket.decode(trailing));
  }
}
