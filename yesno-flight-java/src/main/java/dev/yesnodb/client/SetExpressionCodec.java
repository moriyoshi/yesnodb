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
    } else if (expression instanceof SetExpression.ViewSelect select) {
      output.write(6);
      writeLong(output, select.key());
      writeView(output, select.view());
      writeInt(output, select.set());
    } else if (expression instanceof SetExpression.ViewFold fold) {
      output.write(7);
      writeLong(output, fold.key());
      writeView(output, fold.view());
      output.write(fold.reduce().ordinal());
    } else if (expression instanceof SetExpression.ViewExpand expand) {
      output.write(8);
      writeView(output, expand.view());
      writeExpression(expand.input(), output);
    } else {
      throw new IllegalArgumentException("unknown expression implementation: " + expression);
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
      } else if (expression instanceof SetExpression.ViewExpand expand) {
        visit(expand.input(), depth + 1);
      }
    }
  }

  private static final class Decoder {
    private final ByteBuffer input;
    private int nodes;

    private Decoder(byte[] encoded) {
      input = ByteBuffer.wrap(encoded).order(ByteOrder.LITTLE_ENDIAN);
    }

    private SetExpression decode() {
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
          long key = readLong();
          ViewSpec view = readView();
          long set = readUnsignedInt();
          yield new SetExpression.ViewSelect(key, view, set);
        }
        case 7 -> {
          long key = readLong();
          ViewSpec view = readView();
          int reduce = readUnsignedByte();
          if (reduce >= ViewReduce.values().length) {
            throw malformed("unknown view reduction " + reduce);
          }
          yield new SetExpression.ViewFold(key, view, ViewReduce.values()[reduce]);
        }
        case 8 -> {
          ViewSpec view = readView();
          yield new SetExpression.ViewExpand(readExpression(depth + 1), view);
        }
        case 9 -> readLiteral();
        default -> throw malformed("unknown expression tag " + tag);
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
