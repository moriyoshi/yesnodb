package dev.yesnodb.search;

import java.util.Objects;

/** Builds Elasticsearch query-clause JSON from a prepared yesnodb bitmap. */
public final class ElasticsearchQueryAdapter {
  private ElasticsearchQueryAdapter() {}

  /** Return a native {@code bitmap_terms} clause, or {@code match_none} when empty. */
  public static String queryClauseJson(String field, PreparedBitmap bitmap) {
    Objects.requireNonNull(field, "field");
    Objects.requireNonNull(bitmap, "bitmap");
    if (bitmap.cardinality() == 0) {
      return "{\"match_none\":{}}";
    }
    return "{\"bitmap_terms\":{\"field\":"
        + JsonStrings.quote(field)
        + ",\"value\":"
        + JsonStrings.quote(bitmap.base64())
        + "}}";
  }
}
