//! `search_*`: application helpers and version-locked search plugins.
//!
//! These are ordinary verbs on the one Monty harness. Scenarios retain no
//! filesystem, process, or network authority: the host builds the Java helper,
//! downloads checksum-pinned engine archives, owns disposable installations
//! and processes, and returns only observations the Python oracle needs.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use monty_types::{ExcType, MontyException, MontyObject};
use tempfile::TempDir;

use crate::convert::{int_obj, value_err, Args};
use crate::world::World;

type Result<T> = std::result::Result<T, String>;

const START_TIMEOUT: Duration = Duration::from_secs(90);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAPPING: &str = r#"{"mappings":{"properties":{"ordinal":{"type":"integer"}}}}"#;

/// Populate the build/E2E image with every search integration artifact.
///
/// The engine archives are resolved through the same checksum-verified path as
/// the scenarios. Keeping this here avoids a second list of versions, URLs and
/// digests in the image build.
pub fn prepare_artifacts() -> Result<()> {
    let config = Config::from_env()?;

    let mut application = gradle(&config, "yesno-search-java");
    application.arg("build");
    checked_status(&mut application, "Java search application build")?;

    for engine in [Engine::OpenSearch, Engine::Elasticsearch] {
        build_plugin(&config, engine)?;
        config.archive(engine)?;
    }
    Ok(())
}

/// Search integration verbs dispatched by the ordinary Monty scenario runner.
pub const OWNS: &[&str] = &[
    "search_application",
    "search_engine_start",
    "search_index",
    "search_query",
    "search_engine_stop",
];

#[derive(Default)]
pub struct SearchState {
    engine: Option<RunningEngine>,
}

impl SearchState {
    fn engine_mut(
        &mut self,
        verb: &str,
    ) -> std::result::Result<&mut RunningEngine, MontyException> {
        self.engine.as_mut().ok_or_else(|| {
            value_err(format!(
                "{verb}(): search_engine_start() must be called first"
            ))
        })
    }
}

impl World {
    pub(crate) fn call_search(
        &mut self,
        verb: &str,
        a: &Args<'_>,
    ) -> std::result::Result<MontyObject, MontyException> {
        match verb {
            "search_application" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let location = a.str_at(0)?;
                let config = Config::from_env().map_err(|error| search_err(verb, error))?;
                run_application(&config, location).map_err(|error| search_err(verb, error))?;
                Ok(MontyObject::Bool(true))
            }
            "search_engine_start" => {
                a.exact(2)?;
                a.no_kwargs()?;
                if self.search.engine.is_some() {
                    return Err(value_err(
                        "search_engine_start(): an engine is already running",
                    ));
                }
                let location = a.str_at(0)?;
                let engine = Engine::parse(a.str_at(1)?)?;
                let config = Config::from_env().map_err(|error| search_err(verb, error))?;
                let running = RunningEngine::start(&config, location, engine)
                    .map_err(|error| search_err(verb, error))?;
                self.search.engine = Some(running);
                Ok(MontyObject::String(engine.id().to_owned()))
            }
            "search_index" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let ordinals = a.u64_list(0)?;
                self.search
                    .engine_mut(verb)?
                    .index(&ordinals)
                    .map_err(|error| search_err(verb, error))?;
                Ok(int_obj(ordinals.len() as u64))
            }
            "search_query" => {
                a.exact(1)?;
                a.no_kwargs()?;
                let key = a.u64(0)?;
                let ordinals = self
                    .search
                    .engine_mut(verb)?
                    .query(key)
                    .map_err(|error| search_err(verb, error))?;
                Ok(MontyObject::List(
                    ordinals.into_iter().map(int_obj).collect(),
                ))
            }
            "search_engine_stop" => {
                a.exact(0)?;
                a.no_kwargs()?;
                let mut engine = self.search.engine.take().ok_or_else(|| {
                    value_err("search_engine_stop(): no search engine is running")
                })?;
                engine.stop();
                Ok(MontyObject::Bool(true))
            }
            _ => Err(value_err(format!("{verb}() is not a search verb"))),
        }
    }
}

fn search_err(verb: &str, error: impl std::fmt::Display) -> MontyException {
    MontyException::new(
        ExcType::RuntimeError,
        Some(format!("{verb}(): search integration failed: {error}")),
    )
}

struct Config {
    workspace: PathBuf,
    java_home: PathBuf,
    scratch: PathBuf,
    cache: PathBuf,
    opensearch_override: Option<PathBuf>,
    elasticsearch_override: Option<PathBuf>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .ok_or("yesno-e2e manifest has no workspace parent")?
            .to_path_buf();
        let java_home = std::env::var_os("YESNO_E2E_JAVA_HOME")
            .or_else(|| std::env::var_os("JAVA_HOME"))
            .map(PathBuf::from)
            .ok_or("set YESNO_E2E_JAVA_HOME to a JDK 21 installation")?;
        if !java_home.join("bin/java").is_file() {
            return Err(format!("{} has no bin/java", java_home.display()));
        }

        let scratch = crate::scratch_dir(&workspace);
        fs::create_dir_all(&scratch)
            .map_err(|error| format!("cannot create {}: {error}", scratch.display()))?;
        let cache = scratch.join("search-e2e-cache");
        fs::create_dir_all(&cache)
            .map_err(|error| format!("cannot create {}: {error}", cache.display()))?;

        Ok(Self {
            workspace,
            java_home,
            scratch,
            cache,
            opensearch_override: std::env::var_os("YESNO_E2E_OPENSEARCH_ARCHIVE")
                .map(PathBuf::from),
            elasticsearch_override: std::env::var_os("YESNO_E2E_ELASTICSEARCH_ARCHIVE")
                .map(PathBuf::from),
        })
    }

    fn archive(&self, engine: Engine) -> Result<PathBuf> {
        let supplied = match engine {
            Engine::OpenSearch => self.opensearch_override.as_ref(),
            Engine::Elasticsearch => self.elasticsearch_override.as_ref(),
        };
        let specification = engine.archive()?;
        match supplied {
            Some(path) => {
                verify_archive(path, specification)?;
                Ok(path.clone())
            }
            None => download_archive(&self.cache, specification),
        }
    }
}

fn run_application(config: &Config, flight_location: &str) -> Result<()> {
    let mut command = gradle(config, "yesno-search-java");
    command
        .arg("e2eTest")
        .arg(format!("-PyesnoE2eFlightLocation={flight_location}"));
    checked_status(&mut command, "Java search application E2E")
}

#[derive(Clone, Copy)]
enum Engine {
    OpenSearch,
    Elasticsearch,
}

impl Engine {
    fn parse(value: &str) -> std::result::Result<Self, MontyException> {
        match value {
            "opensearch" => Ok(Self::OpenSearch),
            "elasticsearch" => Ok(Self::Elasticsearch),
            other => Err(value_err(format!(
                "search_engine_start(): unknown engine {other:?}; expected \"opensearch\" or \"elasticsearch\""
            ))),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::OpenSearch => "opensearch",
            Self::Elasticsearch => "elasticsearch",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::OpenSearch => "OpenSearch",
            Self::Elasticsearch => "Elasticsearch",
        }
    }

    fn project(self) -> &'static str {
        match self {
            Self::OpenSearch => "yesno-opensearch-plugin",
            Self::Elasticsearch => "yesno-elasticsearch-plugin",
        }
    }

    fn zip(self, workspace: &Path) -> PathBuf {
        let file = match self {
            Self::OpenSearch => "yesno-opensearch-plugin-3.8.0-0.1.0-SNAPSHOT.zip",
            Self::Elasticsearch => "yesno-elasticsearch-plugin-9.5.2-0.1.0-SNAPSHOT.zip",
        };
        workspace
            .join(self.project())
            .join("build/distributions")
            .join(file)
    }

    fn archive(self) -> Result<&'static Archive> {
        let architecture = std::env::consts::ARCH;
        match (self, architecture) {
            (Self::OpenSearch, "aarch64") => Ok(&OPENSEARCH_ARM64),
            (Self::OpenSearch, "x86_64") => Ok(&OPENSEARCH_X64),
            (Self::Elasticsearch, "aarch64") => Ok(&ELASTICSEARCH_ARM64),
            (Self::Elasticsearch, "x86_64") => Ok(&ELASTICSEARCH_X64),
            _ => Err(format!(
                "{} E2E has no pinned Linux archive for architecture {architecture}",
                self.name()
            )),
        }
    }

    fn executable(self) -> &'static str {
        match self {
            Self::OpenSearch => "opensearch",
            Self::Elasticsearch => "elasticsearch",
        }
    }

    fn installer(self) -> &'static str {
        match self {
            Self::OpenSearch => "opensearch-plugin",
            Self::Elasticsearch => "elasticsearch-plugin",
        }
    }
}

struct RunningEngine {
    child: ChildGuard,
    endpoint: String,
    log_path: PathBuf,
    _work: TempDir,
}

impl RunningEngine {
    fn start(config: &Config, flight_location: &str, engine: Engine) -> Result<Self> {
        build_plugin(config, engine)?;

        let work = tempfile::Builder::new()
            .prefix(&format!("search-{}.", engine.id()))
            .tempdir_in(&config.scratch)
            .map_err(|error| format!("cannot create {} work directory: {error}", engine.name()))?;
        let archive = config.archive(engine)?;
        let distribution = work.path().join("distribution");
        fs::create_dir_all(&distribution)
            .map_err(|error| format!("cannot create {}: {error}", distribution.display()))?;
        let mut tar = Command::new("tar");
        tar.arg("-xf").arg(&archive).arg("-C").arg(&distribution);
        checked_status(&mut tar, &format!("unpack {}", engine.name()))?;
        let home = one_directory(&distribution)?;

        let zip = engine.zip(&config.workspace);
        let mut installer = Command::new(home.join("bin").join(engine.installer()));
        installer
            .env_remove("JAVA_HOME")
            .arg("install")
            .arg("--batch")
            .arg(format!("file://{}", zip.display()));
        checked_status(&mut installer, &format!("install {}", engine.name()))?;

        let port = free_port()?;
        let endpoint = format!("http://127.0.0.1:{port}");
        let log_path = work.path().join(format!("{}.log", engine.id()));
        let log = File::create(&log_path)
            .map_err(|error| format!("cannot create {}: {error}", log_path.display()))?;
        let mut command = Command::new(home.join("bin").join(engine.executable()));
        command
            .env_remove("JAVA_HOME")
            .stdout(Stdio::from(log.try_clone().map_err(|error| {
                format!("cannot clone {}: {error}", log_path.display())
            })?))
            .stderr(Stdio::from(log))
            .arg("-Ediscovery.type=single-node")
            .arg("-Enetwork.host=127.0.0.1")
            .arg(format!("-Ehttp.port={port}"))
            .arg(format!(
                "-Epath.data={}",
                work.path().join("data").display()
            ))
            .arg(format!(
                "-Epath.logs={}",
                work.path().join("logs").display()
            ))
            .arg(format!("-Eyesno.flight.location={flight_location}"));
        match engine {
            Engine::OpenSearch => {
                command
                    .env(
                        "OPENSEARCH_JAVA_OPTS",
                        "-Xms512m -Xmx512m --add-opens=java.base/java.nio=ALL-UNNAMED",
                    )
                    .arg("-Eplugins.security.disabled=true");
            }
            Engine::Elasticsearch => {
                command
                    .env(
                        "ES_JAVA_OPTS",
                        "-Xms512m -Xmx512m --add-opens=java.base/java.nio=ALL-UNNAMED",
                    )
                    .arg("-Expack.security.enabled=false");
            }
        }

        let child = command
            .spawn()
            .map_err(|error| format!("cannot start {}: {error}", engine.name()))?;
        let mut child = ChildGuard::new(child);
        wait_http(&mut child, &endpoint, &log_path, engine.name())?;
        Ok(Self {
            child,
            endpoint,
            log_path,
            _work: work,
        })
    }

    fn index(&mut self, ordinals: &[u64]) -> Result<()> {
        curl_json("PUT", &format!("{}/yesno-e2e", self.endpoint), MAPPING)
            .map_err(|error| self.with_log(error))?;
        let mut bulk = String::new();
        for ordinal in ordinals {
            bulk.push_str(&format!(
                "{{\"index\":{{\"_id\":\"{ordinal}\"}}}}\n{{\"ordinal\":{ordinal}}}\n"
            ));
        }
        let output = curl_json(
            "POST",
            &format!("{}/yesno-e2e/_bulk?refresh=true", self.endpoint),
            &bulk,
        )
        .map_err(|error| self.with_log(error))?;
        let response = String::from_utf8(output.stdout)
            .map_err(|error| self.with_log(format!("bulk response was not UTF-8: {error}")))?;
        if !response.contains("\"errors\":false") {
            return Err(self.with_log(format!("bulk indexing reported an error: {response}")));
        }
        Ok(())
    }

    fn query(&mut self, key: u64) -> Result<Vec<u64>> {
        let body = format!(
            "{{\"size\":10000,\"sort\":[{{\"ordinal\":\"asc\"}}],\"query\":{{\"yesno\":{{\"field\":\"ordinal\",\"width\":\"integer\",\"snapshot\":\"current\",\"expression\":{{\"key\":\"{key}\"}}}}}}}}"
        );
        let output = curl_json(
            "POST",
            &format!(
                "{}/yesno-e2e/_search?filter_path=hits.hits._id",
                self.endpoint
            ),
            &body,
        )
        .map_err(|error| self.with_log(error))?;
        let response = String::from_utf8(output.stdout)
            .map_err(|error| self.with_log(format!("search response was not UTF-8: {error}")))?;
        parse_search_ids(&response).map_err(|error| self.with_log(error))
    }

    fn with_log(&self, error: impl std::fmt::Display) -> String {
        format!("{error}\nengine log:\n{}", read_log(&self.log_path))
    }

    fn stop(&mut self) {
        self.child.stop();
    }
}

fn parse_search_ids(response: &str) -> Result<Vec<u64>> {
    const PREFIX: &str = r#"{"hits":{"hits":["#;
    const SUFFIX: &str = "]}}";
    let inner = response
        .strip_prefix(PREFIX)
        .and_then(|value| value.strip_suffix(SUFFIX))
        .ok_or_else(|| format!("unexpected filtered search response: {response}"))?;
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut values = Vec::new();
    for item in inner.split(',') {
        let raw = item
            .strip_prefix(r#"{"_id":""#)
            .and_then(|value| value.strip_suffix(r#""}"#))
            .ok_or_else(|| format!("unexpected search hit in response: {item}"))?;
        values.push(
            raw.parse::<u64>()
                .map_err(|error| format!("search hit ID {raw:?} is not an ordinal: {error}"))?,
        );
    }
    Ok(values)
}

fn gradle(config: &Config, project: &str) -> Command {
    let mut command = Command::new(config.workspace.join("yesno-flight-java/gradlew"));
    command
        .env("JAVA_HOME", &config.java_home)
        .arg("-p")
        .arg(config.workspace.join(project));
    command
}

fn build_plugin(config: &Config, engine: Engine) -> Result<()> {
    let mut build = gradle(config, engine.project());
    build.arg("build").arg("-PyesnoE2ePermissions=true");
    checked_status(&mut build, &format!("{} plugin build", engine.name()))
}

struct Archive {
    file: &'static str,
    url: &'static str,
    sha512: &'static str,
}

const OPENSEARCH_ARM64: Archive = Archive {
    file: "opensearch-3.8.0-linux-arm64.tar.gz",
    url: "https://artifacts.opensearch.org/releases/bundle/opensearch/3.8.0/opensearch-3.8.0-linux-arm64.tar.gz",
    sha512: "c3d873bbbbce9f08f003b884ec07a4610b9ef30f8916ca12feb13c9863521c5afc61e508ebeabbb565e10b45e8e87e5f2eff5f7e49e39c69375f309d1d476d07",
};
const OPENSEARCH_X64: Archive = Archive {
    file: "opensearch-3.8.0-linux-x64.tar.gz",
    url: "https://artifacts.opensearch.org/releases/bundle/opensearch/3.8.0/opensearch-3.8.0-linux-x64.tar.gz",
    sha512: "cba25b10114e796273fa9399af27fe9c2daaf25a190c37e4b5feb9cfd088e371e0fbd3cddf2bc0fbb753c2e681c0b55f08c0926d11718bf76f77e830017b06ff",
};
const ELASTICSEARCH_ARM64: Archive = Archive {
    file: "elasticsearch-9.5.2-linux-aarch64.tar.gz",
    url: "https://artifacts.elastic.co/downloads/elasticsearch/elasticsearch-9.5.2-linux-aarch64.tar.gz",
    sha512: "73ac07127a41a961edc83daec6dad333df467d8470c812feded6cbdf76cad5db78adbdebe3e8831d6ab52599268bf892e831ff0bf3f760ed987629728a8fa2a6",
};
const ELASTICSEARCH_X64: Archive = Archive {
    file: "elasticsearch-9.5.2-linux-x86_64.tar.gz",
    url: "https://artifacts.elastic.co/downloads/elasticsearch/elasticsearch-9.5.2-linux-x86_64.tar.gz",
    sha512: "f5ff4a6f6e9c00f6a5c46080b053042dfccee04a019bf285d6aee263a738d1046c24556e51be3f8adcef01673a2a10e67a9340d3b5a3d777008de7499cd786f0",
};

/// Download a pinned engine archive into the harness cache.
///
/// Publication is atomic and happens only after the complete temporary file
/// matches the embedded SHA-512. A failed or interrupted transfer is never
/// mistaken for a cached distribution on the next run.
fn download_archive(cache: &Path, specification: &Archive) -> Result<PathBuf> {
    let destination = cache.join(specification.file);
    if destination.is_file() {
        verify_archive(&destination, specification)?;
        return Ok(destination);
    }

    let temporary = tempfile::Builder::new()
        .prefix("download.")
        .tempfile_in(cache)
        .map_err(|error| {
            format!(
                "cannot create a download file in {}: {error}",
                cache.display()
            )
        })?;
    let mut curl = Command::new("curl");
    curl.arg("-fL")
        .arg("--retry")
        .arg("3")
        .arg("--output")
        .arg(temporary.path())
        .arg(specification.url);
    checked_status(&mut curl, &format!("download {}", specification.file))?;
    verify_archive(temporary.path(), specification)?;
    temporary
        .persist(&destination)
        .map_err(|error| format!("cannot publish {}: {}", destination.display(), error.error))?;
    Ok(destination)
}

fn verify_archive(path: &Path, specification: &Archive) -> Result<()> {
    if !path.is_file() {
        return Err(format!("archive does not exist: {}", path.display()));
    }
    let output = Command::new("sha512sum")
        .arg(path)
        .output()
        .map_err(|error| format!("cannot hash {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!("sha512sum failed for {}", path.display()));
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    if actual != specification.sha512 {
        return Err(format!(
            "{} has SHA-512 {actual}, expected {}",
            path.display(),
            specification.sha512
        ));
    }
    Ok(())
}

fn checked_status(command: &mut Command, label: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|error| format!("cannot run {label}: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label} exited with {status}"))
    }
}

fn one_directory(parent: &Path) -> Result<PathBuf> {
    let directories: Vec<_> = fs::read_dir(parent)
        .map_err(|error| format!("cannot read {}: {error}", parent.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    match directories.as_slice() {
        [directory] => Ok(directory.clone()),
        _ => Err(format!(
            "{} contains {} top-level directories instead of one",
            parent.display(),
            directories.len()
        )),
    }
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot reserve a loopback port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|error| format!("cannot inspect the reserved loopback port: {error}"))
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.as_mut().expect("child is present").try_wait()
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

fn wait_http(child: &mut ChildGuard, endpoint: &str, log: &Path, engine: &str) -> Result<()> {
    let deadline = Instant::now() + START_TIMEOUT;
    while Instant::now() < deadline {
        if curl_output(["-fsS", &format!("{endpoint}/")]).is_ok() {
            return Ok(());
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("cannot poll {engine}: {error}"))?
        {
            return Err(format!(
                "{engine} exited with {status} before readiness:\n{}",
                read_log(log)
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Err(format!(
        "{engine} did not become ready at {endpoint}:\n{}",
        read_log(log)
    ))
}

fn curl_json(method: &str, endpoint: &str, body: &str) -> Result<Output> {
    curl_output([
        "--fail-with-body",
        "-sS",
        "-X",
        method,
        endpoint,
        "-H",
        if endpoint.contains("_bulk") {
            "Content-Type: application/x-ndjson"
        } else {
            "Content-Type: application/json"
        },
        "--data-binary",
        body,
    ])
}

fn curl_output<I, S>(arguments: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let output = Command::new("curl")
        .args(arguments)
        .output()
        .map_err(|error| format!("cannot run curl: {error}"))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(format!(
            "curl exited with {}\nresponse: {}\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

fn read_log(path: &Path) -> String {
    let mut text = String::new();
    match File::open(path).and_then(|mut file| file.read_to_string(&mut text)) {
        Ok(_) => text,
        Err(error) => format!("cannot read {}: {error}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filtered_search_ids_are_strictly_parsed() {
        assert_eq!(
            parse_search_ids(r#"{"hits":{"hits":[{"_id":"1"},{"_id":"3"},{"_id":"5"}]}}"#).unwrap(),
            vec![1, 3, 5]
        );
        assert_eq!(
            parse_search_ids(r#"{"hits":{"hits":[]}}"#).unwrap(),
            Vec::<u64>::new()
        );
        assert!(parse_search_ids(r#"{"hits":{"total":3}}"#).is_err());
    }

    #[test]
    fn every_supported_engine_architecture_is_checksum_pinned() {
        for archive in [
            &OPENSEARCH_ARM64,
            &OPENSEARCH_X64,
            &ELASTICSEARCH_ARM64,
            &ELASTICSEARCH_X64,
        ] {
            assert_eq!(archive.sha512.len(), 128);
            assert!(archive.sha512.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(archive.url.ends_with(archive.file));
        }
    }
}
