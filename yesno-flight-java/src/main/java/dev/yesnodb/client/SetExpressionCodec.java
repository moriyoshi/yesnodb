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

  static byte[] encodeBig(BigExpression expression) {
    ByteArrayOutputStream output = new ByteArrayOutputStream(32);
    output.writeBytes(MAGIC);
    output.write(VERSION);
    output.write(0);
    writeBig(expression, output);
    return output.toByteArray();
  }

  static BigExpression decodeBig(byte[] encoded) {
    return new Decoder(encoded).decodeBig();
  }

  static byte[] encodeBigVector(VecBigExpression vector) {
    ByteArrayOutputStream output = new ByteArrayOutputStream(32);
    output.writeBytes(MAGIC);
    output.write(VERSION);
    output.write(0);
    writeBigVector(vector, output);
    return output.toByteArray();
  }

  static VecBigExpression decodeBigVector(byte[] encoded) {
    return new Decoder(encoded).decodeBigVector();
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
      case 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 38, 39 -> "big integer";
      case 36, 37 -> "vector of big integers";
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

  private static void writeBig(BigExpression expression, ByteArrayOutputStream output) {
    if (expression instanceof BigExpression.Literal literal) {
      output.write(24);
      // Sign and canonical little-endian magnitude: no trailing zero byte, and
      // a negative zero cannot occur, so one value has exactly one encoding.
      java.math.BigInteger magnitude = literal.value().abs();
      byte[] body = littleEndianMagnitude(magnitude);
      output.write(literal.value().signum() < 0 ? 1 : 0);
      writeInt(output, body.length);
      output.writeBytes(body);
    } else if (expression instanceof BigExpression.Widen widen) {
      output.write(25);
      writeIntExpr(widen.input(), output);
    } else if (expression instanceof BigExpression.ReadUint read) {
      output.write(26);
      writeInt(output, read.widthBits());
      writeExpression(read.input(), output);
    } else if (expression instanceof BigExpression.ReadInt read) {
      output.write(27);
      writeInt(output, read.widthBits());
      writeExpression(read.input(), output);
    } else if (expression instanceof BigExpression.Negate negate) {
      output.write(28);
      writeBig(negate.input(), output);
    } else if (expression instanceof BigExpression.Add add) {
      writeBigPair(29, add.left(), add.right(), output);
    } else if (expression instanceof BigExpression.Subtract sub) {
      writeBigPair(30, sub.left(), sub.right(), output);
    } else if (expression instanceof BigExpression.Multiply mul) {
      writeBigPair(31, mul.left(), mul.right(), output);
    } else if (expression instanceof BigExpression.Divide div) {
      writeBigPair(32, div.left(), div.right(), output);
    } else if (expression instanceof BigExpression.Remainder rem) {
      writeBigPair(33, rem.left(), rem.right(), output);
    } else if (expression instanceof BigExpression.Truncate truncate) {
      output.write(34);
      writeInt(output, truncate.bits());
      writeBig(truncate.input(), output);
    } else if (expression instanceof BigExpression.Saturate saturate) {
      output.write(35);
      writeInt(output, saturate.bits());
      writeBig(saturate.input(), output);
    } else if (expression instanceof BigExpression.Fold fold) {
      output.write(38);
      output.write(fold.op().wire());
      writeBigVector(fold.vector(), output);
    } else if (expression instanceof BigExpression.PowMod pow) {
      output.write(39);
      writeBig(pow.base(), output);
      writeBig(pow.exp(), output);
      writeBig(pow.modulus(), output);
    } else {
      throw new IllegalArgumentException("unknown big-integer implementation: " + expression);
    }
  }

  private static void writeBigPair(
      int tag, BigExpression left, BigExpression right, ByteArrayOutputStream output) {
    output.write(tag);
    writeBig(left, output);
    writeBig(right, output);
  }

  /** A magnitude as little-endian bytes with no trailing zero. */
  private static byte[] littleEndianMagnitude(java.math.BigInteger magnitude) {
    if (magnitude.signum() == 0) {
      return new byte[0];
    }
    byte[] be = magnitude.toByteArray();
    int start = 0;
    while (start < be.length - 1 && be[start] == 0) {
      start++;
    }
    int length = be.length - start;
    byte[] le = new byte[length];
    for (int i = 0; i < length; i++) {
      le[i] = be[be.length - 1 - i];
    }
    return le;
  }

  private static void writeBigVector(VecBigExpression vector, ByteArrayOutputStream output) {
    if (vector instanceof VecBigExpression.Listing list) {
      output.write(36);
      writeShort(output, list.elements().size());
      list.elements().forEach(element -> writeBig(element, output));
    } else if (vector instanceof VecBigExpression.MapBig map) {
      output.write(37);
      writeVector(map.vector(), output);
      writeBig(map.body(), output);
    } else {
      throw new IllegalArgumentException("unknown big-vector implementation: " + vector);
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

    private BigExpression readBigBody(int depth) {
      enterBody();
      try {
        return readBig(depth);
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

    private BigExpression readBig(int depth) {
      countNode(depth);
      int tag = readUnsignedByte();
      BigExpression out =
          switch (tag) {
            case 24 -> {
              int sign = readUnsignedByte();
              if (sign > 1) {
                throw malformed("unknown sign byte " + sign);
              }
              long length = readUnsignedInt();
              // Checked before the bytes are taken, so an over-wide length is
              // refused as the amplification it is rather than as a truncation.
              if (length * 8 > BigExpression.MAX_VALUE_BITS) {
                throw malformed(
                    "value is wider than " + BigExpression.MAX_VALUE_BITS + " bits");
              }
              require((int) length);
              byte[] body = new byte[(int) length];
              input.get(body);
              if (body.length > 0 && body[body.length - 1] == 0) {
                throw malformed("big-integer literal is not canonical");
              }
              if (sign == 1 && body.length == 0) {
                throw malformed("big-integer literal is not canonical");
              }
              java.math.BigInteger magnitude = bigEndianOf(body);
              yield new BigExpression.Literal(sign == 1 ? magnitude.negate() : magnitude);
            }
            case 25 -> new BigExpression.Widen(readInt(depth + 1));
            case 26 -> {
              int width = readWidth();
              yield new BigExpression.ReadUint(readExpression(depth + 1), width);
            }
            case 27 -> {
              int width = readWidth();
              yield new BigExpression.ReadInt(readExpression(depth + 1), width);
            }
            case 28 -> new BigExpression.Negate(readBig(depth + 1));
            case 29 -> new BigExpression.Add(readBig(depth + 1), readBig(depth + 1));
            case 30 -> new BigExpression.Subtract(readBig(depth + 1), readBig(depth + 1));
            case 31 -> new BigExpression.Multiply(readBig(depth + 1), readBig(depth + 1));
            case 32 -> new BigExpression.Divide(readBig(depth + 1), readBig(depth + 1));
            case 33 -> new BigExpression.Remainder(readBig(depth + 1), readBig(depth + 1));
            case 34 -> {
              int bits = readBits();
              yield new BigExpression.Truncate(readBig(depth + 1), bits);
            }
            case 35 -> {
              int bits = readBits();
              yield new BigExpression.Saturate(readBig(depth + 1), bits);
            }
            case 39 ->
                new BigExpression.PowMod(
                    readBig(depth + 1), readBig(depth + 1), readBig(depth + 1));
            case 38 -> {
              byte raw = (byte) readUnsignedByte();
              BigFoldOp op = BigFoldOp.fromWire(raw);
              if (op == null) {
                throw malformed("unknown fold operator " + raw);
              }
              yield new BigExpression.Fold(readBigVector(depth + 1), op);
            }
            default -> throw misplaced("big integer", tag);
          };
      // Per node rather than only at the root: a sub-expression cannot be
      // wider than the whole is allowed to be.
      if (out.widthBound() > BigExpression.MAX_VALUE_BITS) {
        throw malformed("value is wider than " + BigExpression.MAX_VALUE_BITS + " bits");
      }
      if (out.workBound() > BigExpression.MAX_WORK) {
        throw malformed(
            "query asks for more than " + BigExpression.MAX_WORK + " limb operations");
      }
      return out;
    }

    private int readWidth() {
      long width = readUnsignedInt();
      if (width == 0) {
        throw malformed("a big-integer read has zero width");
      }
      if (width > BigExpression.MAX_VALUE_BITS) {
        throw malformed("value is wider than " + BigExpression.MAX_VALUE_BITS + " bits");
      }
      return (int) width;
    }

    /** A zero width is meaningful for the narrowing nodes, unlike on a read. */
    private int readBits() {
      long bits = readUnsignedInt();
      if (bits > BigExpression.MAX_VALUE_BITS) {
        throw malformed("value is wider than " + BigExpression.MAX_VALUE_BITS + " bits");
      }
      return (int) bits;
    }

    private static java.math.BigInteger bigEndianOf(byte[] littleEndian) {
      byte[] be = new byte[littleEndian.length + 1];
      for (int i = 0; i < littleEndian.length; i++) {
        be[be.length - 1 - i] = littleEndian[i];
      }
      return new java.math.BigInteger(be);
    }

    private VecBigExpression readBigVector(int depth) {
      countNode(depth);
      int tag = readUnsignedByte();
      VecBigExpression out =
          switch (tag) {
            case 36 -> {
              int count = readUnsignedShort();
              if (count == 0) {
                throw malformed("vector has no elements");
              }
              if (nodes + count > SetExpression.MAX_NODES) {
                throw malformed(
                    "expression has more than " + SetExpression.MAX_NODES + " nodes");
              }
              List<BigExpression> elements = new ArrayList<>(count);
              for (int index = 0; index < count; index++) {
                elements.add(readBig(depth + 1));
              }
              yield new VecBigExpression.Listing(elements);
            }
            case 37 -> {
              VecSetExpression vector = readVector(depth + 1);
              yield new VecBigExpression.MapBig(vector, readBigBody(depth + 1));
            }
            default -> throw misplaced("vector of big integers", tag);
          };
      // The product bound, checked where both factors are in hand.
      if (out.resultBound() > VecBigExpression.MAX_RESULT_BITS) {
        throw malformed(
            "result is larger than " + VecBigExpression.MAX_RESULT_BITS + " bits");
      }
      if (out.workBound() > BigExpression.MAX_WORK) {
        throw malformed(
            "query asks for more than " + BigExpression.MAX_WORK + " limb operations");
      }
      return out;
    }

    private BigExpression decodeBig() {
      readHeader();
      BigExpression value = readBig(0);
      if (input.hasRemaining()) {
        throw malformed("trailing bytes after expression");
      }
      return value;
    }

    private VecBigExpression decodeBigVector() {
      readHeader();
      VecBigExpression vector = readBigVector(0);
      if (input.hasRemaining()) {
        throw malformed("trailing bytes after expression");
      }
      return vector;
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
