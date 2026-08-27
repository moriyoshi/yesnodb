package dev.yesnodb.elasticsearch;

import static org.junit.jupiter.api.Assertions.assertEquals;

import dev.yesnodb.search.BitmapWidth;
import dev.yesnodb.search.PreparedBitmap;
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.List;
import org.apache.lucene.analysis.standard.StandardAnalyzer;
import org.apache.lucene.document.Document;
import org.apache.lucene.document.NumericDocValuesField;
import org.apache.lucene.index.DirectoryReader;
import org.apache.lucene.index.IndexWriter;
import org.apache.lucene.index.IndexWriterConfig;
import org.apache.lucene.search.IndexSearcher;
import org.apache.lucene.search.ScoreDoc;
import org.apache.lucene.store.ByteBuffersDirectory;
import org.junit.jupiter.api.Test;
import org.roaringbitmap.RoaringBitmap;
import org.roaringbitmap.longlong.Roaring64NavigableMap;

class PortableBitmapDocValuesQueryTest {
  @Test
  void matchesPortableIntegerAndLongBitmaps() throws Exception {
    try (ByteBuffersDirectory directory = new ByteBuffersDirectory()) {
      writeDocuments(directory);
      try (DirectoryReader reader = DirectoryReader.open(directory)) {
        IndexSearcher searcher = new IndexSearcher(reader);
        assertEquals(List.of(0, 2, 4), matches(searcher, prepared32(1, 3, 5)));
        assertEquals(List.of(0, 2, 4), matches(searcher, prepared64(1, 3, 5)));
        assertEquals(List.of(), matches(searcher, prepared64()));
      }
    }
  }

  private static void writeDocuments(ByteBuffersDirectory directory) throws IOException {
    try (IndexWriter writer =
        new IndexWriter(directory, new IndexWriterConfig(new StandardAnalyzer()))) {
      for (long value = 1; value <= 5; value++) {
        Document document = new Document();
        document.add(new NumericDocValuesField("product_id", value));
        writer.addDocument(document);
      }
      Document negative = new Document();
      negative.add(new NumericDocValuesField("product_id", -1));
      writer.addDocument(negative);
    }
  }

  private static List<Integer> matches(IndexSearcher searcher, PreparedBitmap bitmap)
      throws IOException {
    ScoreDoc[] hits =
        searcher.search(PortableBitmapDocValuesQuery.from("product_id", bitmap), 10).scoreDocs;
    List<Integer> docIds = new ArrayList<>(hits.length);
    for (ScoreDoc hit : hits) {
      docIds.add(hit.doc);
    }
    return docIds;
  }

  private static PreparedBitmap prepared32(int... values) throws IOException {
    RoaringBitmap bitmap = RoaringBitmap.bitmapOf(values);
    ByteArrayOutputStream bytes = new ByteArrayOutputStream();
    try (DataOutputStream output = new DataOutputStream(bytes)) {
      bitmap.serialize(output);
    }
    return new PreparedBitmap(BitmapWidth.INTEGER_32, 1, bitmap.getLongCardinality(), bytes.toByteArray());
  }

  private static PreparedBitmap prepared64(long... values) throws IOException {
    Roaring64NavigableMap bitmap = new Roaring64NavigableMap();
    for (long value : values) {
      bitmap.addLong(value);
    }
    ByteArrayOutputStream bytes = new ByteArrayOutputStream();
    try (DataOutputStream output = new DataOutputStream(bytes)) {
      bitmap.serializePortable(output);
    }
    return new PreparedBitmap(BitmapWidth.LONG_64, 1, bitmap.getLongCardinality(), bytes.toByteArray());
  }
}
