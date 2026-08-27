package dev.yesnodb.opensearch;

import dev.yesnodb.client.SetExpression;
import dev.yesnodb.client.YesnoClient;
import dev.yesnodb.search.BitmapWidth;
import dev.yesnodb.search.PreparedBitmap;
import dev.yesnodb.search.ResolveLimits;
import dev.yesnodb.search.SnapshotMode;
import dev.yesnodb.search.YesnoBitmapResolver;
import java.net.URI;
import java.util.Objects;
import java.util.concurrent.Executor;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import io.grpc.LoadBalancerRegistry;
import io.grpc.internal.PickFirstLoadBalancerProvider;
import org.apache.arrow.flight.CallOption;
import org.apache.arrow.flight.CallOptions;
import org.apache.arrow.flight.Location;
import org.opensearch.common.settings.Settings;
import org.opensearch.core.action.ActionListener;

final class YesnoOpenSearchService {
  private static final AtomicReference<YesnoOpenSearchService> INSTANCE = new AtomicReference<>();

  private final Location location;
  private final ResolveLimits limits;
  private final int currentRetries;
  private final long timeoutMillis;
  private final Executor executor;

  YesnoOpenSearchService(Settings settings, Executor executor) {
    registerShadedGrpcProviders();
    location = new Location(URI.create(YesnoOpenSearchPlugin.FLIGHT_LOCATION.get(settings)));
    limits =
        new ResolveLimits(
            YesnoOpenSearchPlugin.MAX_MATCHES.get(settings),
            YesnoOpenSearchPlugin.MAX_BITMAP_BYTES.get(settings));
    currentRetries = YesnoOpenSearchPlugin.CURRENT_RETRIES.get(settings);
    timeoutMillis = YesnoOpenSearchPlugin.FLIGHT_TIMEOUT.get(settings).millis();
    this.executor = Objects.requireNonNull(executor, "executor");
  }

  private static void registerShadedGrpcProviders() {
    LoadBalancerRegistry registry = LoadBalancerRegistry.getDefaultRegistry();
    if (registry.getProvider("pick_first") == null) {
      registry.register(new PickFirstLoadBalancerProvider());
    }
  }

  static void install(YesnoOpenSearchService service) {
    if (!INSTANCE.compareAndSet(null, Objects.requireNonNull(service, "service"))) {
      throw new IllegalStateException("yesno OpenSearch service is already installed");
    }
  }

  static YesnoOpenSearchService instance() {
    YesnoOpenSearchService service = INSTANCE.get();
    if (service == null) {
      throw new IllegalStateException("yesno OpenSearch service is not initialized");
    }
    return service;
  }

  static void uninstall(YesnoOpenSearchService service) {
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
