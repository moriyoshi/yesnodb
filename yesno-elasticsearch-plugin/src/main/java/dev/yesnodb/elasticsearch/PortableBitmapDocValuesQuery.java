/*
 * SPDX-License-Identifier: Apache-2.0
 *
 * Adapted from the OpenSearch BitmapDocValuesQuery and Bitmap64DocValuesQuery
 * implementations for the yesnodb Elasticsearch plugin.
 */
package dev.yesnodb.elasticsearch;

import dev.yesnodb.search.BitmapWidth;
import dev.yesnodb.search.PreparedBitmap;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.util.Objects;
import org.apache.lucene.index.DocValues;
import org.apache.lucene.index.LeafReaderContext;
import org.apache.lucene.index.NumericDocValues;
import org.apache.lucene.index.SortedNumericDocValues;
import org.apache.lucene.search.ConstantScoreScorer;
import org.apache.lucene.search.ConstantScoreWeight;
import org.apache.lucene.search.IndexSearcher;
import org.apache.lucene.search.MatchNoDocsQuery;
import org.apache.lucene.search.Query;
import org.apache.lucene.search.QueryVisitor;
import org.apache.lucene.search.ScoreMode;
import org.apache.lucene.search.Scorer;
import org.apache.lucene.search.ScorerSupplier;
import org.apache.lucene.search.TwoPhaseIterator;
import org.apache.lucene.search.Weight;
import org.apache.lucene.util.Accountable;
import org.apache.lucene.util.RamUsageEstimator;
import org.roaringbitmap.RoaringBitmap;
import org.roaringbitmap.longlong.Roaring64NavigableMap;

/** Constant-score membership query over sorted numeric doc values and a portable Roaring bitmap. */
final class PortableBitmapDocValuesQuery extends Query implements Accountable {
  private final String field;
  private final BitmapWidth width;
  private final RoaringBitmap bitmap32;
  private final Roaring64NavigableMap bitmap64;
  private final long min;
  private final long max;

  static PortableBitmapDocValuesQuery from(String field, PreparedBitmap prepared)
      throws IOException {
    byte[] bytes = prepared.portableBytes();
    if (prepared.width() == BitmapWidth.INTEGER_32) {
      RoaringBitmap bitmap = new RoaringBitmap();
      bitmap.deserialize(ByteBuffer.wrap(bytes));
      return new PortableBitmapDocValuesQuery(field, bitmap);
    }
    Roaring64NavigableMap bitmap = new Roaring64NavigableMap();
    bitmap.deserializePortable(new DataInputStream(new ByteArrayInputStream(bytes)));
    return new PortableBitmapDocValuesQuery(field, bitmap);
  }

  private PortableBitmapDocValuesQuery(String field, RoaringBitmap bitmap) {
    this.field = Objects.requireNonNull(field, "field");
    width = BitmapWidth.INTEGER_32;
    bitmap32 = Objects.requireNonNull(bitmap, "bitmap");
    bitmap64 = null;
    min = bitmap.isEmpty() ? 0 : bitmap.first();
    max = bitmap.isEmpty() ? 0 : bitmap.last();
  }

  private PortableBitmapDocValuesQuery(String field, Roaring64NavigableMap bitmap) {
    this.field = Objects.requireNonNull(field, "field");
    width = BitmapWidth.LONG_64;
    bitmap32 = null;
    bitmap64 = Objects.requireNonNull(bitmap, "bitmap");
    min = bitmap.isEmpty() ? 0 : bitmap.first();
    max = bitmap.isEmpty() ? 0 : bitmap.last();
  }

  @Override
  public Weight createWeight(IndexSearcher searcher, ScoreMode scoreMode, float boost) {
    return new ConstantScoreWeight(this, boost) {
      @Override
      public ScorerSupplier scorerSupplier(LeafReaderContext context) throws IOException {
        SortedNumericDocValues values = DocValues.getSortedNumeric(context.reader(), field);
        NumericDocValues singleton = DocValues.unwrapSingleton(values);
        TwoPhaseIterator iterator =
            singleton == null ? multiValued(values) : singleValued(singleton);
        Scorer scorer = new ConstantScoreScorer(score(), scoreMode, iterator);
        return new Weight.DefaultScorerSupplier(scorer);
      }

      @Override
      public boolean isCacheable(LeafReaderContext context) {
        return DocValues.isCacheable(context, field);
      }
    };
  }

  private TwoPhaseIterator singleValued(NumericDocValues values) {
    return new TwoPhaseIterator(values) {
      @Override
      public boolean matches() throws IOException {
        long value = values.longValue();
        return value >= min && value <= max && contains(value);
      }

      @Override
      public float matchCost() {
        return 5;
      }
    };
  }

  private TwoPhaseIterator multiValued(SortedNumericDocValues values) {
    return new TwoPhaseIterator(values) {
      @Override
      public boolean matches() throws IOException {
        int count = values.docValueCount();
        for (int index = 0; index < count; index++) {
          long value = values.nextValue();
          if (value < min) {
            continue;
          }
          if (value > max) {
            return false;
          }
          if (contains(value)) {
            return true;
          }
        }
        return false;
      }

      @Override
      public float matchCost() {
        return 5;
      }
    };
  }

  private boolean contains(long value) {
    return width == BitmapWidth.INTEGER_32
        ? value >= 0 && value <= Integer.MAX_VALUE && bitmap32.contains((int) value)
        : value >= 0 && bitmap64.contains(value);
  }

  private boolean isEmpty() {
    return width == BitmapWidth.INTEGER_32 ? bitmap32.isEmpty() : bitmap64.isEmpty();
  }

  @Override
  public Query rewrite(IndexSearcher searcher) throws IOException {
    return isEmpty() ? new MatchNoDocsQuery("yesno bitmap is empty") : super.rewrite(searcher);
  }

  @Override
  public void visit(QueryVisitor visitor) {
    if (visitor.acceptField(field)) {
      visitor.visitLeaf(this);
    }
  }

  @Override
  public String toString(String defaultField) {
    return "PortableBitmapDocValuesQuery(field=" + field + ",width=" + width + ")";
  }

  @Override
  public boolean equals(Object other) {
    if (!sameClassAs(other)) {
      return false;
    }
    PortableBitmapDocValuesQuery query = (PortableBitmapDocValuesQuery) other;
    return field.equals(query.field)
        && width == query.width
        && Objects.equals(bitmap32, query.bitmap32)
        && Objects.equals(bitmap64, query.bitmap64);
  }

  @Override
  public int hashCode() {
    return Objects.hash(classHash(), field, width, bitmap32, bitmap64);
  }

  @Override
  public long ramBytesUsed() {
    long bitmapBytes =
        width == BitmapWidth.INTEGER_32
            ? bitmap32.getLongSizeInBytes()
            : bitmap64.getLongSizeInBytes();
    return RamUsageEstimator.shallowSizeOfInstance(PortableBitmapDocValuesQuery.class)
        + RamUsageEstimator.sizeOf(field)
        + bitmapBytes;
  }
}
