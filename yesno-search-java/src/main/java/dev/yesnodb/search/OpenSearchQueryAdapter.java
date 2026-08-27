package dev.yesnodb.search;

import java.util.Objects;

/** Builds OpenSearch query-clause JSON from a prepared yesnodb bitmap. */
public final class OpenSearchQueryAdapter {
  private OpenSearchQueryAdapter() {}

  /** Return a native bitmap-valued {@code terms} clause, or {@code match_none} when empty. */
  public static String queryClauseJson(String field, PreparedBitmap bitmap) {
    Objects.requireNonNull(field, "field");
    Objects.requireNonNull(bitmap, "bitmap");
    if (bitmap.cardinality() == 0) {
      return "{\"match_none\":{}}";
    }
    String quotedField = JsonStrings.quote(field);
    String quotedBitmap = JsonStrings.quote(bitmap.base64());
    return "{\"terms\":{" + quotedField + ":[" + quotedBitmap + "],\"value_type\":\"bitmap\"}}";
  }
}
