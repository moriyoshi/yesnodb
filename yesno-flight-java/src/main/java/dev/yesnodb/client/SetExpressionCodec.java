package dev.yesnodb.client;

import java.io.ByteArrayOutputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.List;

final class SetExpressionCodec {
  private static final byte[] MAGIC = {'Y', 'S', 'N', 'X'};
  private static final int VERSION = 1;

  private SetExpressionCodec() {}

  static byte[] encode(SetExpression expression) {
    new Validator().visit(expression, 0);
    ByteArrayOutputStream output = new ByteArrayOutputStream(32);
    output.writeBytes(MAGIC);
    output.write(VERSION);
    output.write(0);
    writeExpression(expression, output);
    return output.toByteArray();
  }

  static SetExpression decode(byte[] encoded) {
    return new Decoder(encoded).decode();
  }

  static byte[] encodeIntVector(VecIntExpression vector) {
    new Validator().visitIntVector(vector, 0);
    ByteArrayOutputStream output = new ByteArrayOutputStream(32);
    output.writeBytes(MAGIC);
    output.write(VERSION);
    output.write(0);
    writeIntVector(vector, output);
    return output.toByteArray();
  }

  static VecIntExpression decodeIntVector(byte[] encoded) {
    return new Decoder(encoded).decodeIntVector();
  }

  /**
   * The sort a tag belongs to, or {@code null} if no version defines it.
   *
   * <p>One table, consulted by every decoder's fallback. Enumerating other sorts' tags inside each
   * arm is how a tag ends up reported as unknown in one position and as a sort mismatch in another.
   */
  private static String sortOfTag(int tag) {
    return switch (tag) {
      case 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 13, 14, 17 -> "set";
      case 11, 12, 15 -> "vector of sets";
      case 16, 22 -> "vector of integers";
      case 18, 19, 20, 21 -> "integer";
      case 23 -> "boolean";
      default -> null;
    };
  }

  private static void writeExpression(SetExpression expression, ByteArrayOutputStream output) {
    if (expression instanceof SetExpression.Empty) {
      output.write(0);
    } else if (expression instanceof SetExpression.Key key) {
      output.write(1);
      writeLong(output, key.key());
    } else if (expression instanceof SetExpression.Range range) {
      output.write(2);
      writeLong(output, range.lo());
      writeLong(output, range.hi());
    } else if (expression instanceof SetExpression.Literal literal) {
      output.write(9);
      writeInt(output, literal.ordinals().size());
      literal.ordinals().forEach(ordinal -> writeLong(output, ordinal));
    } else if (expression instanceof SetExpression.And and) {
      output.write(3);
      writeJunction(and.operands(), output);
    } else if (expression instanceof SetExpression.Or or) {
      output.write(4);
      writeJunction(or.operands(), output);
    } else if (expression instanceof SetExpression.AndNot andNot) {
      output.write(5);
      writeExpression(andNot.include(), output);
      writeExpression(andNot.exclude(), output);
    } else if (expression instanceof SetExpression.At at) {
      output.write(6);
      writeVector(at.vector(), output);
      writeInt(output, at.index());
    } else if (expression instanceof SetExpression.Fold fold) {
      output.write(7);
      writeVector(fold.vector(), output);
      output.write(fold.op().ordinal());
    } else if (expression instanceof SetExpression.Pack pack) {
      output.write(10);
      writeView(output, pack.view());
      writeVector(pack.vector(), output);
    } else if (expression instanceof SetExpression.Expand expand) {
      output.write(8);
      writeView(output, expand.view());
      writeExpression(expand.input(), output);
    } else if (expression instanceof SetExpression.Hole) {
      output.write(13);
    } else if (expression instanceof SetExpression.Select select) {
      output.write(14);
      writeLong(output, select.index());
      writeExpression(select.input(), output);
    } else if (expression instanceof SetExpression.MapBool map) {
      output.write(17);
      writeVector(map.vector(), output);
      writeBool(map.body(), output);
    } else {
      throw new IllegalArgumentException("unknown expression implementation: " + expression);
    }
  }

  private static void writeVector(VecSetExpression vector, ByteArrayOutputStream output) {
    if (vector instanceof VecSetExpression.Listing list) {
      output.write(11);
      writeShort(output, list.elements().size());
      list.elements().forEach(element -> writeExpression(element, output));
    } else if (vector instanceof VecSetExpression.View view) {
      output.write(12);
      writeView(output, view.view());
      writeExpression(view.input(), output);
    } else if (vector instanceof VecSetExpression.MapSet map) {
      output.write(15);
      writeVector(map.vector(), output);
      writeExpression(map.body(), output);
    } else {
      throw new IllegalArgumentException("unknown vector implementation: " + vector);
    }
  }

  private static void writeBool(BoolExpression expression, ByteArrayOutputStream output) {
    if (expression instanceof BoolExpression.Contains contains) {
      output.write(23);
      writeLong(output, contains.ordinal());
      writeExpression(contains.input(), output);
    } else {
      throw new IllegalArgumentException("unknown boolean implementation: " + expression);
    }
  }

  private static void writeIntExpr(IntExpression expression, ByteArrayOutputStream output) {
    if (expression instanceof IntExpression.Literal literal) {
      output.write(20);
      writeLong(output, literal.value());
    } else if (expression instanceof IntExpression.Cardinality cardinality) {
      output.write(18);
      writeExpression(cardinality.input(), output);
    } else if (expression instanceof IntExpression.Rank rank) {
      output.write(19);
      writeLong(output, rank.position());
      writeExpression(rank.input(), output);
    } else if (expression instanceof IntExpression.At at) {
      output.write(21);
      writeIntVector(at.vector(), output);
      writeInt(output, at.index());
    } else {
      throw new IllegalArgumentException("unknown integer implementation: " + expression);
    }
  }

  private static void writeIntVector(VecIntExpression vector, ByteArrayOutputStream output) {
    if (vector instanceof VecIntExpression.Listing list) {
      output.write(22);
      writeShort(output, list.elements().size());
      list.elements().forEach(element -> writeIntExpr(element, output));
    } else if (vector instanceof VecIntExpression.MapInt map) {
      output.write(16);
      writeVector(map.vector(), output);
      writeIntExpr(map.body(), output);
    } else {
      throw new IllegalArgumentException("unknown vector implementation: " + vector);
    }
  }

  private static void writeJunction(List<SetExpression> operands, ByteArrayOutputStream output) {
    writeShort(output, operands.size());
    operands.forEach(operand -> writeExpression(operand, output));
  }

  private static void writeView(ByteArrayOutputStream output, ViewSpec view) {
    writeInt(output, view.sets());
    output.write(view.layout() == ViewSpec.Layout.INTERLEAVED ? 0 : 1);
    writeLong(output, view.stride());
  }

  private static void writeShort(ByteArrayOutputStream output, long value) {
    output.write((int) value & 0xff);
    output.write((int) (value >>> 8) & 0xff);
  }

  private static void writeInt(ByteArrayOutputStream output, long value) {
    for (int shift = 0; shift < Integer.SIZE; shift += Byte.SIZE) {
      output.write((int) (value >>> shift) & 0xff);
    }
  }

  private static void writeLong(ByteArrayOutputStream output, long value) {
    for (int shift = 0; shift < Long.SIZE; shift += Byte.SIZE) {
      output.write((int) (value >>> shift) & 0xff);
    }
  }

  private static final class Validator {
    private int nodes;

    private void visit(SetExpression expression, int depth) {
      if (depth > SetExpression.MAX_DEPTH) {
        throw new IllegalArgumentException(
            "expression nested deeper than " + SetExpression.MAX_DEPTH);
      }
      nodes++;
      if (nodes > SetExpression.MAX_NODES) {
        throw new IllegalArgumentException(
            "expression has more than " + SetExpression.MAX_NODES + " nodes");
      }
      if (expression instanceof SetExpression.And and) {
        and.operands().forEach(child -> visit(child, depth + 1));
      } else if (expression instanceof SetExpression.Or or) {
        or.operands().forEach(child -> visit(child, depth + 1));
      } else if (expression instanceof SetExpression.AndNot andNot) {
        visit(andNot.include(), depth + 1);
        visit(andNot.exclude(), depth + 1);
      } else if (expression instanceof SetExpression.Expand expand) {
        visit(expand.input(), depth + 1);
      } else if (expression instanceof SetExpression.At at) {
        visitVector(at.vector(), depth + 1);
      } else if (expression instanceof SetExpression.Fold fold) {
        visitVector(fold.vector(), depth + 1);
      } else if (expression instanceof SetExpression.Pack pack) {
        visitVector(pack.vector(), depth + 1);
      } else if (expression instanceof SetExpression.Select select) {
        visit(select.input(), depth + 1);
      } else if (expression instanceof SetExpression.MapBool map) {
        visitVector(map.vector(), depth + 1);
        visitBool(map.body(), depth + 1);
      }
    }

    private void visitBool(BoolExpression expression, int depth) {
      count(depth);
      if (expression instanceof BoolExpression.Contains contains) {
        visit(contains.input(), depth + 1);
      }
    }

    private void visitInt(IntExpression expression, int depth) {
      count(depth);
      if (expression instanceof IntExpression.Cardinality cardinality) {
        visit(cardinality.input(), depth + 1);
      } else if (expression instanceof IntExpression.Rank rank) {
        visit(rank.input(), depth + 1);
      } else if (expression instanceof IntExpression.At at) {
        visitIntVector(at.vector(), depth + 1);
      }
    }

    void visitIntVector(VecIntExpression vector, int depth) {
      count(depth);
      if (vector instanceof VecIntExpression.Listing list) {
        if (list.elements().size() > 0xffff) {
          throw new IllegalArgumentException("vector has more than 65535 elements");
        }
        list.elements().forEach(element -> visitInt(element, depth + 1));
      } else if (vector instanceof VecIntExpression.MapInt map) {
        visitVector(map.vector(), depth + 1);
        visitInt(map.body(), depth + 1);
      }
    }

    private void count(int depth) {
      if (depth > SetExpression.MAX_DEPTH) {
        throw new IllegalArgumentException(
            "expression nested deeper than " + SetExpression.MAX_DEPTH);
      }
      nodes++;
      if (nodes > SetExpression.MAX_NODES) {
        throw new IllegalArgumentException(
            "expression has more than " + SetExpression.MAX_NODES + " nodes");
      }
    }

    private void visitVector(VecSetExpression vector, int depth) {
      if (depth > SetExpression.MAX_DEPTH) {
        throw new IllegalArgumentException(
            "expression nested deeper than " + SetExpression.MAX_DEPTH);
      }
      nodes++;
      if (nodes > SetExpression.MAX_NODES) {
        throw new IllegalArgumentException(
            "expression has more than " + SetExpression.MAX_NODES + " nodes");
      }
      if (vector instanceof VecSetExpression.Listing list) {
        list.elements().forEach(element -> visit(element, depth + 1));
      } else if (vector instanceof VecSetExpression.View view) {
        visit(view.input(), depth + 1);
      } else if (vector instanceof VecSetExpression.MapSet map) {
        visitVector(map.vector(), depth + 1);
        visit(map.body(), depth + 1);
      }
    }
  }

  private static final class Decoder {
    private final ByteBuffer input;
    private int nodes;

    /** Whether decoding is inside a map body. See {@link #enterBody()}. */
    private boolean inMap;

    private Decoder(byte[] encoded) {
      input = ByteBuffer.wrap(encoded).order(ByteOrder.LITTLE_ENDIAN);
    }

    private void readHeader() {
      if (input.remaining() < 6) {
        throw malformed("expression ended in its header");
      }
      for (byte expected : MAGIC) {
        if (input.get() != expected) {
          throw malformed("not a yesnodb expression");
        }
      }
      int version = Byte.toUnsignedInt(input.get());
      int reserved = Byte.toUnsignedInt(input.get());
      if (version != VERSION || reserved != 0) {
        throw malformed("unsupported expression version " + version);
      }
    }

    private SetExpression decode() {
      readHeader();
      SetExpression expression = readExpression(0);
      if (input.hasRemaining()) {
        throw malformed("trailing bytes after expression");
      }
      return expression;
    }

    private SetExpression readExpression(int depth) {
      if (depth > SetExpression.MAX_DEPTH) {
        throw malformed("expression nested deeper than " + SetExpression.MAX_DEPTH);
      }
      nodes++;
      if (nodes > SetExpression.MAX_NODES) {
        throw malformed("expression has more than " + SetExpression.MAX_NODES + " nodes");
      }
      int tag = readUnsignedByte();
      return switch (tag) {
        case 0 -> new SetExpression.Empty();
        case 1 -> new SetExpression.Key(readLong());
        case 2 -> new SetExpression.Range(readLong(), readLong());
        case 3 -> new SetExpression.And(readOperands(depth));
        case 4 -> new SetExpression.Or(readOperands(depth));
        case 5 -> new SetExpression.AndNot(readExpression(depth + 1), readExpression(depth + 1));
        case 6 -> {
          VecSetExpression vector = readVector(depth + 1);
          long index = readUnsignedInt();
          if (index >= vector.arity()) {
            throw malformed("index is at or above the vector's arity");
          }
          yield new SetExpression.At(vector, index);
        }
        case 7 -> {
          VecSetExpression vector = readVector(depth + 1);
          int op = readUnsignedByte();
          if (op >= FoldOp.values().length) {
            throw malformed("unknown fold operator " + op);
          }
          yield new SetExpression.Fold(vector, FoldOp.values()[op]);
        }
        case 8 -> {
          ViewSpec view = readView();
          yield new SetExpression.Expand(readExpression(depth + 1), view);
        }
        case 9 -> readLiteral();
        case 10 -> {
          ViewSpec view = readView();
          VecSetExpression vector = readVector(depth + 1);
          if (vector.arity() != view.sets()) {
            throw malformed(
                "packing " + vector.arity() + " sets under a " + view.sets() + "-set descriptor");
          }
          yield new SetExpression.Pack(vector, view);
        }
        case 13 -> {
          if (!inMap) {
            throw malformed("`_` outside a map body");
          }
          yield new SetExpression.Hole();
        }
        case 14 -> {
          long index = readLong();
          yield new SetExpression.Select(readExpression(depth + 1), index);
        }
        case 17 -> {
          VecSetExpression vector = readVector(depth + 1);
          yield new SetExpression.MapBool(vector, readBoolBody(depth + 1));
        }
        default -> throw misplaced("set", tag);
      };
    }

    /** A known tag of another sort is a sort mismatch; an undefined one is unknown. */
    private IllegalArgumentException misplaced(String expected, int tag) {
      if (sortOfTag(tag) != null) {
        return malformed("tag " + tag + " where a " + expected + " was required");
      }
      return malformed("unknown expression tag " + tag);
    }

    private BoolExpression readBoolBody(int depth) {
      enterBody();
      try {
        return readBool(depth);
      } finally {
        inMap = false;
      }
    }

    private IntExpression readIntBody(int depth) {
      enterBody();
      try {
        return readInt(depth);
      } finally {
        inMap = false;
      }
    }

    private SetExpression readSetBody(int depth) {
      enterBody();
      try {
        return readExpression(depth);
      } finally {
        inMap = false;
      }
    }

    /**
     * A map body is the only place a hole is legal, and it may not contain a map.
     *
     * <p>Refusing nesting rather than tracking it is what lets the hole stay un-indexed. A map in a
     * <em>vector</em> position is sequential rather than nested, and is unaffected.
     */
    private void enterBody() {
      if (inMap) {
        throw malformed("a map body may not contain a map");
      }
      inMap = true;
    }

    private BoolExpression readBool(int depth) {
      countNode(depth);
      int tag = readUnsignedByte();
      if (tag != 23) {
        throw misplaced("boolean", tag);
      }
      long ordinal = readLong();
      return new BoolExpression.Contains(readExpression(depth + 1), ordinal);
    }

    private IntExpression readInt(int depth) {
      countNode(depth);
      int tag = readUnsignedByte();
      return switch (tag) {
        case 18 -> new IntExpression.Cardinality(readExpression(depth + 1));
        case 19 -> {
          long position = readLong();
          yield new IntExpression.Rank(readExpression(depth + 1), position);
        }
        case 20 -> new IntExpression.Literal(readLong());
        case 21 -> {
          VecIntExpression vector = readIntVector(depth + 1);
          long index = readUnsignedInt();
          if (index >= vector.arity()) {
            throw malformed("index is at or above the vector's arity");
          }
          yield new IntExpression.At(vector, index);
        }
        default -> throw misplaced("integer", tag);
      };
    }

    private VecIntExpression readIntVector(int depth) {
      countNode(depth);
      int tag = readUnsignedByte();
      return switch (tag) {
        case 16 -> {
          VecSetExpression vector = readVector(depth + 1);
          yield new VecIntExpression.MapInt(vector, readIntBody(depth + 1));
        }
        case 22 -> {
          int count = readUnsignedShort();
          if (count == 0) {
            throw malformed("vector has no elements");
          }
          if (nodes + count > SetExpression.MAX_NODES) {
            throw malformed("expression has more than " + SetExpression.MAX_NODES + " nodes");
          }
          List<IntExpression> elements = new ArrayList<>(count);
          for (int index = 0; index < count; index++) {
            elements.add(readInt(depth + 1));
          }
          yield new VecIntExpression.Listing(elements);
        }
        default -> throw misplaced("vector of integers", tag);
      };
    }

    private void countNode(int depth) {
      if (depth > SetExpression.MAX_DEPTH) {
        throw malformed("expression nested deeper than " + SetExpression.MAX_DEPTH);
      }
      nodes++;
      if (nodes > SetExpression.MAX_NODES) {
        throw malformed("expression has more than " + SetExpression.MAX_NODES + " nodes");
      }
    }

    private VecIntExpression decodeIntVector() {
      readHeader();
      VecIntExpression vector = readIntVector(0);
      if (input.hasRemaining()) {
        throw malformed("trailing bytes after expression");
      }
      return vector;
    }

    /**
     * Decode a vector-sorted node.
     *
     * <p>The mirror of {@code readExpression}, and the reason both tag ranges share one space: a
     * set-sorted tag arriving here is a sort error naming both sides, not a reinterpretation of
     * whatever that byte means in this position.
     */
    private VecSetExpression readVector(int depth) {
      if (depth > SetExpression.MAX_DEPTH) {
        throw malformed("expression nested deeper than " + SetExpression.MAX_DEPTH);
      }
      nodes++;
      if (nodes > SetExpression.MAX_NODES) {
        throw malformed("expression has more than " + SetExpression.MAX_NODES + " nodes");
      }
      int tag = readUnsignedByte();
      return switch (tag) {
        case 11 -> {
          int count = readUnsignedShort();
          if (count == 0) {
            throw malformed("vector has no elements");
          }
          if (nodes + count > SetExpression.MAX_NODES) {
            throw malformed("expression has more than " + SetExpression.MAX_NODES + " nodes");
          }
          List<SetExpression> elements = new ArrayList<>(count);
          for (int index = 0; index < count; index++) {
            elements.add(readExpression(depth + 1));
          }
          yield new VecSetExpression.Listing(elements);
        }
        case 12 -> {
          ViewSpec view = readView();
          yield new VecSetExpression.View(readExpression(depth + 1), view);
        }
        case 15 -> {
          VecSetExpression vector = readVector(depth + 1);
          yield new VecSetExpression.MapSet(vector, readSetBody(depth + 1));
        }
        default -> throw misplaced("vector of sets", tag);
      };
    }

    private SetExpression readLiteral() {
      long count = readUnsignedInt();
      if (count > input.remaining() / Long.BYTES) {
        throw malformed("expression ended mid-node");
      }
      List<Long> ordinals = new ArrayList<>((int) count);
      for (long index = 0; index < count; index++) {
        long ordinal = readLong();
        if (ordinal == -1L) {
          throw malformed("18446744073709551615 is outside the ordinal universe");
        }
        if (!ordinals.isEmpty()
            && Long.compareUnsigned(ordinals.get(ordinals.size() - 1), ordinal) >= 0) {
          throw malformed("ordinal-set literal is not strictly ascending and unique");
        }
        ordinals.add(ordinal);
      }
      return new SetExpression.Literal(ordinals);
    }

    private List<SetExpression> readOperands(int depth) {
      int count = readUnsignedShort();
      if (count == 0) {
        throw malformed("AND/OR has no operands");
      }
      List<SetExpression> operands = new ArrayList<>(count);
      for (int index = 0; index < count; index++) {
        operands.add(readExpression(depth + 1));
      }
      return operands;
    }

    private ViewSpec readView() {
      long sets = readUnsignedInt();
      int layout = readUnsignedByte();
      long stride = readLong();
      try {
        return switch (layout) {
          case 0 -> new ViewSpec(sets, ViewSpec.Layout.INTERLEAVED, stride);
          case 1 -> new ViewSpec(sets, ViewSpec.Layout.BLOCKED, stride);
          default -> throw malformed("unknown view layout " + layout);
        };
      } catch (IllegalArgumentException exception) {
        throw malformed(exception.getMessage());
      }
    }

    private int readUnsignedByte() {
      require(Byte.BYTES);
      return Byte.toUnsignedInt(input.get());
    }

    private int readUnsignedShort() {
      require(Short.BYTES);
      return Short.toUnsignedInt(input.getShort());
    }

    private long readUnsignedInt() {
      require(Integer.BYTES);
      return Integer.toUnsignedLong(input.getInt());
    }

    private long readLong() {
      require(Long.BYTES);
      return input.getLong();
    }

    private void require(int bytes) {
      if (input.remaining() < bytes) {
        throw malformed("expression ended mid-node");
      }
    }

    private static IllegalArgumentException malformed(String message) {
      return new IllegalArgumentException(message);
    }
  }
}
