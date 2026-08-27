package dev.yesnodb.opensearch;

import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.UnsignedLongs;
import dev.yesnodb.search.BitmapWidth;
import dev.yesnodb.search.OpenSearchQueryAdapter;
import dev.yesnodb.search.PreparedBitmap;
import dev.yesnodb.search.SetExpressionMaps;
import java.io.IOException;
import java.util.HashSet;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.function.Supplier;
import org.apache.lucene.search.Query;
import org.opensearch.common.SetOnce;
import org.opensearch.core.action.ActionListener;
import org.opensearch.core.common.ParsingException;
import org.opensearch.core.common.io.stream.StreamInput;
import org.opensearch.core.common.io.stream.StreamOutput;
import org.opensearch.core.xcontent.XContentBuilder;
import org.opensearch.core.xcontent.XContentParser;
import org.opensearch.index.query.AbstractQueryBuilder;
import org.opensearch.index.query.QueryBuilder;
import org.opensearch.index.query.QueryRewriteContext;
import org.opensearch.index.query.QueryShardContext;
import org.opensearch.index.query.WrapperQueryBuilder;

/** Coordinator-rewritten yesnodb bitmap filter for OpenSearch 3.8. */
public final class YesnoOpenSearchQueryBuilder
    extends AbstractQueryBuilder<YesnoOpenSearchQueryBuilder> {
  public static final String NAME = "yesno";

  private static final Set<String> FIELDS =
      Set.of("field", "expression", "width", "snapshot", "boost", "_name");

  private final String field;
  private final SetExpression expression;
  private final BitmapWidth width;
  private final Long pinnedVersion;
  private final Supplier<PreparedBitmap> supplier;

  YesnoOpenSearchQueryBuilder(
      String field, SetExpression expression, BitmapWidth width, Long pinnedVersion) {
    this(field, expression, width, pinnedVersion, null);
  }

  private YesnoOpenSearchQueryBuilder(
      String field,
      SetExpression expression,
      BitmapWidth width,
      Long pinnedVersion,
      Supplier<PreparedBitmap> supplier) {
    this.field = Objects.requireNonNull(field, "field");
    if (field.isBlank()) {
      throw new IllegalArgumentException("field must not be blank");
    }
    this.expression = Objects.requireNonNull(expression, "expression");
    this.width = Objects.requireNonNull(width, "width");
    this.pinnedVersion = pinnedVersion;
    this.supplier = supplier;
  }

  /** Deserialize a builder transported between OpenSearch nodes. */
  public YesnoOpenSearchQueryBuilder(StreamInput input) throws IOException {
    super(input);
    field = input.readString();
    expression = SetExpression.decode(input.readByteArray());
    width = input.readEnum(BitmapWidth.class);
    pinnedVersion = input.readBoolean() ? input.readLong() : null;
    supplier = null;
  }

  /** Parse the body of a {@code yesno} query. */
  public static YesnoOpenSearchQueryBuilder fromXContent(XContentParser parser) throws IOException {
    Map<String, Object> body = parser.mapOrdered();
    Set<String> unknown = new HashSet<>(body.keySet());
    unknown.removeAll(FIELDS);
    if (!unknown.isEmpty()) {
      throw new ParsingException(parser.getTokenLocation(), "[yesno] unknown fields " + unknown);
    }

    Object fieldValue = body.get("field");
    if (!(fieldValue instanceof String field)) {
      throw new ParsingException(parser.getTokenLocation(), "[yesno] field must be a string");
    }
    Object expressionValue = body.get("expression");
    if (expressionValue == null) {
      throw new ParsingException(parser.getTokenLocation(), "[yesno] expression is required");
    }

    YesnoOpenSearchQueryBuilder builder =
        new YesnoOpenSearchQueryBuilder(
            field,
            SetExpressionMaps.parse(expressionValue),
            parseWidth(body.getOrDefault("width", "long")),
            parseSnapshot(body.getOrDefault("snapshot", "current")));
    if (body.containsKey("boost")) {
      Object boost = body.get("boost");
      if (!(boost instanceof Number number)) {
        throw new ParsingException(parser.getTokenLocation(), "[yesno] boost must be numeric");
      }
      builder.boost(number.floatValue());
    }
    if (body.containsKey("_name")) {
      Object queryName = body.get("_name");
      if (!(queryName instanceof String name)) {
        throw new ParsingException(parser.getTokenLocation(), "[yesno] _name must be a string");
      }
      builder.queryName(name);
    }
    return builder;
  }

  private static BitmapWidth parseWidth(Object value) {
    if ("integer".equals(value)) {
      return BitmapWidth.INTEGER_32;
    }
    if ("long".equals(value)) {
      return BitmapWidth.LONG_64;
    }
    throw new IllegalArgumentException("width must be `integer` or `long`");
  }

  private static Long parseSnapshot(Object value) {
    if ("current".equals(value)) {
      return null;
    }
    if (value instanceof Map<?, ?> object
        && object.size() == 1
        && object.get("version") != null) {
      return UnsignedLongs.parse(object.get("version").toString());
    }
    throw new IllegalArgumentException("snapshot must be `current` or an object containing `version`");
  }

  @Override
  protected void doWriteTo(StreamOutput output) throws IOException {
    output.writeString(field);
    output.writeByteArray(expression.encode());
    output.writeEnum(width);
    output.writeBoolean(pinnedVersion != null);
    if (pinnedVersion != null) {
      output.writeLong(pinnedVersion);
    }
  }

  @Override
  protected void doXContent(XContentBuilder builder, Params params) throws IOException {
    builder.startObject(NAME);
    builder.field("field", field);
    builder.field("expression", SetExpressionMaps.toMap(expression));
    builder.field("width", width == BitmapWidth.INTEGER_32 ? "integer" : "long");
    if (pinnedVersion == null) {
      builder.field("snapshot", "current");
    } else {
      builder.startObject("snapshot");
      builder.field("version", UnsignedLongs.toString(pinnedVersion));
      builder.endObject();
    }
    printBoostAndQueryName(builder);
    builder.endObject();
  }

  @Override
  protected Query doToQuery(QueryShardContext context) {
    throw new UnsupportedOperationException("yesno queries must be rewritten on the coordinator");
  }

  @Override
  protected QueryBuilder doRewrite(QueryRewriteContext context) {
    if (supplier != null) {
      PreparedBitmap bitmap = supplier.get();
      return bitmap == null
          ? this
          : new WrapperQueryBuilder(OpenSearchQueryAdapter.queryClauseJson(field, bitmap));
    }
    if (context.convertToCoordinatorContext() == null) {
      return this;
    }

    SetOnce<PreparedBitmap> result = new SetOnce<>();
    context.registerAsyncAction(
        (client, listener) ->
            YesnoOpenSearchService.instance()
                .resolve(
                    expression,
                    width,
                    pinnedVersion,
                    ActionListener.map(
                        listener,
                        bitmap -> {
                          result.set(bitmap);
                          return null;
                        })));
    return new YesnoOpenSearchQueryBuilder(field, expression, width, pinnedVersion, result::get);
  }

  @Override
  protected int doHashCode() {
    return Objects.hash(field, expression, width, pinnedVersion, supplier);
  }

  @Override
  protected boolean doEquals(YesnoOpenSearchQueryBuilder other) {
    return field.equals(other.field)
        && expression.equals(other.expression)
        && width == other.width
        && Objects.equals(pinnedVersion, other.pinnedVersion)
        && Objects.equals(supplier, other.supplier);
  }

  @Override
  public String getWriteableName() {
    return NAME;
  }
}
