package dev.yesnodb.elasticsearch;

import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.YesnoClient;
import dev.yesnodb.search.BitmapWidth;
import dev.yesnodb.search.PreparedBitmap;
import dev.yesnodb.search.ResolveLimits;
import dev.yesnodb.search.SnapshotMode;
import dev.yesnodb.search.YesnoBitmapResolver;
import io.grpc.LoadBalancerRegistry;
import io.grpc.internal.PickFirstLoadBalancerProvider;
import java.net.URI;
import java.util.Objects;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import org.apache.arrow.flight.CallOption;
import org.apache.arrow.flight.CallOptions;
import org.apache.arrow.flight.Location;
import org.elasticsearch.action.ActionListener;
import org.elasticsearch.common.settings.Settings;

final class YesnoElasticsearchService {
  private static final AtomicReference<YesnoElasticsearchService> INSTANCE = new AtomicReference<>();

  private final Location location;
  private final ResolveLimits limits;
  private final int currentRetries;
  private final long timeoutMillis;
  private final Executor executor;

  YesnoElasticsearchService(Settings settings, Executor executor) {
    registerShadedGrpcProviders();
    location = new Location(URI.create(YesnoElasticsearchPlugin.FLIGHT_LOCATION.get(settings)));
    limits =
        new ResolveLimits(
            YesnoElasticsearchPlugin.MAX_MATCHES.get(settings),
            YesnoElasticsearchPlugin.MAX_BITMAP_BYTES.get(settings));
    currentRetries = YesnoElasticsearchPlugin.CURRENT_RETRIES.get(settings);
    timeoutMillis = YesnoElasticsearchPlugin.FLIGHT_TIMEOUT.get(settings).millis();
    this.executor = Objects.requireNonNull(executor, "executor");
  }

  private static void registerShadedGrpcProviders() {
    LoadBalancerRegistry registry = LoadBalancerRegistry.getDefaultRegistry();
    if (registry.getProvider("pick_first") == null) {
      registry.register(new PickFirstLoadBalancerProvider());
    }
  }

  static void install(YesnoElasticsearchService service) {
    if (!INSTANCE.compareAndSet(null, Objects.requireNonNull(service, "service"))) {
      throw new IllegalStateException("yesno Elasticsearch service is already installed");
    }
  }

  static YesnoElasticsearchService instance() {
    YesnoElasticsearchService service = INSTANCE.get();
    if (service == null) {
      throw new IllegalStateException("yesno Elasticsearch service is not initialized");
    }
    return service;
  }

  static void uninstall(YesnoElasticsearchService service) {
    INSTANCE.compareAndSet(service, null);
  }

  void resolve(
      SetExpression expression,
      BitmapWidth width,
      Long pinnedVersion,
      ActionListener<PreparedBitmap> listener) {
    executor.execute(
        () -> {
          try {
            listener.onResponse(resolveBlocking(expression, width, pinnedVersion));
          } catch (Exception exception) {
            listener.onFailure(exception);
          } catch (LinkageError error) {
            listener.onFailure(new IllegalStateException("could not initialize Arrow Flight", error));
          }
        });
  }

  private PreparedBitmap resolveBlocking(
      SetExpression expression, BitmapWidth width, Long pinnedVersion) {
    SnapshotMode snapshot =
        pinnedVersion == null
            ? new SnapshotMode.Current(currentRetries)
            : new SnapshotMode.Pinned(pinnedVersion);
    CallOption timeout = CallOptions.timeout(timeoutMillis, TimeUnit.MILLISECONDS);
    try (YesnoClient client = YesnoClient.connect(location)) {
      return new YesnoBitmapResolver(client).resolve(expression, width, snapshot, limits, timeout);
    }
  }
}
