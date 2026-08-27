package dev.yesnodb.search;

import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.UnsignedLongs;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/** Parses the engine-neutral map form used by the search plugins into a yesnodb expression. */
public final class SetExpressionMaps {
  private SetExpressionMaps() {}

  /** Parse one expression map, enforcing yesnodb's public depth and node limits. */
  public static SetExpression parse(Object value) {
    Counter counter = new Counter();
    return parseNode(value, 1, counter);
  }

  /** Convert the core Boolean expression subset back to its engine-neutral map form. */
  public static Map<String, Object> toMap(SetExpression expression) {
    if (expression instanceof SetExpression.Empty) {
      return Map.of("empty", Map.of());
    }
    if (expression instanceof SetExpression.Key key) {
      return Map.of("key", UnsignedLongs.toString(key.key()));
    }
    if (expression instanceof SetExpression.Range range) {
      return Map.of(
          "range",
          orderedPair(
              "lo", UnsignedLongs.toString(range.lo()),
              "hi", UnsignedLongs.toString(range.hi())));
    }
    if (expression instanceof SetExpression.Literal literal) {
      return Map.of(
          "literal",
          literal.ordinals().stream().map(UnsignedLongs::toString).toList());
    }
    if (expression instanceof SetExpression.And and) {
      return Map.of("and", and.operands().stream().map(SetExpressionMaps::toMap).toList());
    }
    if (expression instanceof SetExpression.Or or) {
      return Map.of("or", or.operands().stream().map(SetExpressionMaps::toMap).toList());
    }
    if (expression instanceof SetExpression.AndNot andNot) {
      return Map.of(
          "and_not",
          orderedPair(
              "include", toMap(andNot.include()),
              "exclude", toMap(andNot.exclude())));
    }
    return unsupportedViewExpression();
  }

  private static SetExpression parseNode(Object value, int depth, Counter counter) {
    if (depth > SetExpression.MAX_DEPTH) {
      throw new IllegalArgumentException(
          "expression exceeds the maximum depth of " + SetExpression.MAX_DEPTH);
    }
    counter.nodes++;
    if (counter.nodes > SetExpression.MAX_NODES) {
      throw new IllegalArgumentException(
          "expression exceeds the maximum node count of " + SetExpression.MAX_NODES);
    }
    if (!(value instanceof Map<?, ?> node) || node.size() != 1) {
      throw new IllegalArgumentException("each expression node must be an object with one operator");
    }

    Map.Entry<?, ?> entry = node.entrySet().iterator().next();
    if (!(entry.getKey() instanceof String operator)) {
      throw new IllegalArgumentException("expression operator names must be strings");
    }
    Object operand = entry.getValue();
    return switch (operator) {
      case "empty" -> parseEmpty(operand);
      case "key" -> SetExpression.key(unsigned(operand, "key"));
      case "range" -> parseRange(operand);
      case "literal" -> parseLiteral(operand);
      case "and" -> new SetExpression.And(parseOperands(operand, depth, counter, "and"));
      case "or" -> new SetExpression.Or(parseOperands(operand, depth, counter, "or"));
      case "and_not" -> parseAndNot(operand, depth, counter);
      default -> throw new IllegalArgumentException("unknown expression operator: " + operator);
    };
  }

  private static SetExpression parseEmpty(Object operand) {
    if (!(operand instanceof Map<?, ?> object) || !object.isEmpty()) {
      throw new IllegalArgumentException("empty must contain an empty object");
    }
    return SetExpression.empty();
  }

  private static SetExpression parseRange(Object operand) {
    Map<?, ?> object = exactObject(operand, "range", "lo", "hi");
    return SetExpression.range(unsigned(object.get("lo"), "range.lo"), unsigned(object.get("hi"), "range.hi"));
  }

  private static SetExpression parseLiteral(Object operand) {
    if (!(operand instanceof List<?> values)) {
      throw new IllegalArgumentException("literal must contain an array of ordinals");
    }
    long[] ordinals = new long[values.size()];
    for (int index = 0; index < values.size(); index++) {
      long ordinal = unsigned(values.get(index), "literal ordinal");
      if (ordinal == -1L) {
        throw new IllegalArgumentException(
            "18446744073709551615 is outside the ordinal universe");
      }
      ordinals[index] = ordinal;
    }
    return SetExpression.literal(ordinals);
  }

  private static SetExpression parseAndNot(Object operand, int depth, Counter counter) {
    Map<?, ?> object = exactObject(operand, "and_not", "include", "exclude");
    return SetExpression.andNot(
        parseNode(object.get("include"), depth + 1, counter),
        parseNode(object.get("exclude"), depth + 1, counter));
  }

  private static List<SetExpression> parseOperands(
      Object operand, int depth, Counter counter, String operator) {
    if (!(operand instanceof List<?> values) || values.isEmpty()) {
      throw new IllegalArgumentException(operator + " must contain a non-empty array");
    }
    List<SetExpression> expressions = new ArrayList<>(values.size());
    for (Object value : values) {
      expressions.add(parseNode(value, depth + 1, counter));
    }
    return expressions;
  }

  private static Map<?, ?> exactObject(
      Object value, String label, String first, String second) {
    if (!(value instanceof Map<?, ?> object)
        || object.size() != 2
        || !object.containsKey(first)
        || !object.containsKey(second)) {
      throw new IllegalArgumentException(
          label + " must contain exactly `" + first + "` and `" + second + "`");
    }
    return object;
  }

  private static long unsigned(Object value, String label) {
    if (!(value instanceof String) && !(value instanceof Byte) && !(value instanceof Short)
        && !(value instanceof Integer) && !(value instanceof Long)) {
      throw new IllegalArgumentException(label + " must be an unsigned decimal string or integer");
    }
    try {
      return UnsignedLongs.parse(value.toString());
    } catch (NumberFormatException exception) {
      throw new IllegalArgumentException(label + " is outside the unsigned 64-bit range", exception);
    }
  }

  private static Map<String, Object> orderedPair(
      String firstKey, Object firstValue, String secondKey, Object secondValue) {
    Map<String, Object> result = new LinkedHashMap<>();
    result.put(firstKey, firstValue);
    result.put(secondKey, secondValue);
    return result;
  }

  private static Map<String, Object> unsupportedViewExpression() {
    throw new IllegalArgumentException("the search plugin map form does not support packed-view nodes");
  }

  private static final class Counter {
    private int nodes;
  }
}
