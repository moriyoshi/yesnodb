package dev.yesnodb.elasticsearch;

import java.util.Collection;
import java.util.List;
import org.elasticsearch.common.settings.Setting;
import org.elasticsearch.common.settings.Settings;
import org.elasticsearch.common.util.concurrent.EsExecutors;
import org.elasticsearch.core.TimeValue;
import org.elasticsearch.plugins.Plugin;
import org.elasticsearch.plugins.SearchPlugin;
import org.elasticsearch.threadpool.ExecutorBuilder;
import org.elasticsearch.threadpool.FixedExecutorBuilder;

/** Elasticsearch 9.5 plugin exposing the {@code yesno} query. */
public final class YesnoElasticsearchPlugin extends Plugin implements SearchPlugin {
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
  private YesnoElasticsearchService service;

  /** Construct the plugin from node settings. */
  public YesnoElasticsearchPlugin(Settings settings) {
    this.settings = settings;
  }

  @Override
  public List<QuerySpec<?>> getQueries() {
    return List.of(
        new QuerySpec<>(
            YesnoElasticsearchQueryBuilder.NAME,
            YesnoElasticsearchQueryBuilder::new,
            YesnoElasticsearchQueryBuilder::fromXContent));
  }

  @Override
  public List<Setting<?>> getSettings() {
    return List.of(FLIGHT_LOCATION, FLIGHT_TIMEOUT, MAX_MATCHES, MAX_BITMAP_BYTES, CURRENT_RETRIES);
  }

  @Override
  public List<ExecutorBuilder<?>> getExecutorBuilders(Settings nodeSettings) {
    return List.of(
        new FixedExecutorBuilder(
            nodeSettings,
            EXECUTOR_NAME,
            2,
            100,
            "thread_pool." + EXECUTOR_NAME,
            EsExecutors.TaskTrackingConfig.DO_NOT_TRACK));
  }

  @Override
  public Collection<?> createComponents(PluginServices services) {
    service = new YesnoElasticsearchService(settings, services.threadPool().executor(EXECUTOR_NAME));
    YesnoElasticsearchService.install(service);
    return List.of(service);
  }

  @Override
  public void close() {
    if (service != null) {
      YesnoElasticsearchService.uninstall(service);
    }
  }
}
