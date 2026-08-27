package dev.yesnodb.opensearch;

import java.util.Collection;
import java.util.List;
import java.util.function.Supplier;
import org.opensearch.cluster.metadata.IndexNameExpressionResolver;
import org.opensearch.cluster.service.ClusterService;
import org.opensearch.common.settings.Setting;
import org.opensearch.common.settings.Settings;
import org.opensearch.common.unit.TimeValue;
import org.opensearch.core.xcontent.NamedXContentRegistry;
import org.opensearch.core.common.io.stream.NamedWriteableRegistry;
import org.opensearch.env.Environment;
import org.opensearch.env.NodeEnvironment;
import org.opensearch.plugins.Plugin;
import org.opensearch.plugins.SearchPlugin;
import org.opensearch.repositories.RepositoriesService;
import org.opensearch.script.ScriptService;
import org.opensearch.threadpool.ExecutorBuilder;
import org.opensearch.threadpool.FixedExecutorBuilder;
import org.opensearch.threadpool.ThreadPool;
import org.opensearch.transport.client.Client;
import org.opensearch.watcher.ResourceWatcherService;

/** OpenSearch 3.8 plugin exposing the {@code yesno} query. */
public final class YesnoOpenSearchPlugin extends Plugin implements SearchPlugin {
  static final String EXECUTOR_NAME = "yesno_resolve";

  static final Setting<String> FLIGHT_LOCATION =
      Setting.simpleString(
          "yesno.flight.location", "grpc+tcp://127.0.0.1:50051", Setting.Property.NodeScope);
  static final Setting<TimeValue> FLIGHT_TIMEOUT =
      Setting.timeSetting(
          "yesno.flight.timeout", TimeValue.timeValueSeconds(5), Setting.Property.NodeScope);
  static final Setting<Long> MAX_MATCHES =
      Setting.longSetting("yesno.max_matches", 10_000_000L, 0, Setting.Property.NodeScope);
  static final Setting<Long> MAX_BITMAP_BYTES =
      Setting.longSetting(
          "yesno.max_bitmap_bytes", 64L * 1024 * 1024, 0, Setting.Property.NodeScope);
  static final Setting<Integer> CURRENT_RETRIES =
      Setting.intSetting("yesno.current_retries", 1, 0, Setting.Property.NodeScope);

  private final Settings settings;
  private YesnoOpenSearchService service;

  /** Construct the plugin from node settings. */
  public YesnoOpenSearchPlugin(Settings settings) {
    this.settings = settings;
  }

  @Override
  public List<QuerySpec<?>> getQueries() {
    return List.of(
        new QuerySpec<>(
            YesnoOpenSearchQueryBuilder.NAME,
            YesnoOpenSearchQueryBuilder::new,
            YesnoOpenSearchQueryBuilder::fromXContent));
  }

  @Override
  public List<Setting<?>> getSettings() {
    return List.of(FLIGHT_LOCATION, FLIGHT_TIMEOUT, MAX_MATCHES, MAX_BITMAP_BYTES, CURRENT_RETRIES);
  }

  @Override
  public List<ExecutorBuilder<?>> getExecutorBuilders(Settings nodeSettings) {
    return List.of(
        new FixedExecutorBuilder(
            nodeSettings, EXECUTOR_NAME, 2, 100, "thread_pool." + EXECUTOR_NAME));
  }

  @Override
  public Collection<Object> createComponents(
      Client client,
      ClusterService clusterService,
      ThreadPool threadPool,
      ResourceWatcherService resourceWatcherService,
      ScriptService scriptService,
      NamedXContentRegistry xContentRegistry,
      Environment environment,
      NodeEnvironment nodeEnvironment,
      NamedWriteableRegistry namedWriteableRegistry,
      IndexNameExpressionResolver indexNameExpressionResolver,
      Supplier<RepositoriesService> repositoriesServiceSupplier) {
    service = new YesnoOpenSearchService(settings, threadPool.executor(EXECUTOR_NAME));
    YesnoOpenSearchService.install(service);
    return List.of(service);
  }

  @Override
  public void close() {
    if (service != null) {
      YesnoOpenSearchService.uninstall(service);
    }
  }
}
