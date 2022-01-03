//! `observer` is a program that inspects a prometheus with a configured query,
//! writes the result out to disk.
use argh::FromArgs;
use prometheus_parser::GroupKind;
use reqwest::Url;
use serde::Deserialize;
use snafu::Snafu;
use std::{
    borrow::Cow,
    fs,
    io::{Read, Write},
    time::{Duration, SystemTime, SystemTimeError, UNIX_EPOCH},
};
use tokio::{runtime::Builder, time::sleep};
use tracing::{debug, error};
use uuid::Uuid;

fn default_config_path() -> String {
    "/etc/vector/soak/observer.yaml".to_string()
}

#[derive(FromArgs)]
/// vector soak `observer` options
struct Opts {
    /// path on disk to the configuration file
    #[argh(option, default = "default_config_path()")]
    config_path: String,
}

/// Target configuration
#[derive(Debug, Deserialize)]
pub struct Target {
    id: String,
    url: String,
}

/// Main configuration struct for this program
#[derive(Debug, Deserialize)]
pub struct Config {
    /// The name of the experiment being observed
    pub experiment_name: String,
    /// The variant of the experiment, generally 'baseline' or 'comparison'
    pub variant: soak::Variant,
    /// The targets for this experiment.
    pub targets: Vec<Target>,
    /// The file to record captures into
    pub capture_path: String,
}

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("Reqwest error: {}", error))]
    Reqwest { error: reqwest::Error },
    #[snafu(display("Not a valid URI with error: {}", error))]
    Http { error: http::uri::InvalidUri },
    #[snafu(display("IO error: {}", error))]
    Io { error: std::io::Error },
    #[snafu(display("Could not parse float: {}", error))]
    ParseFloat { error: std::num::ParseFloatError },
    #[snafu(display("Could not serialize output: {}", error))]
    Json { error: serde_json::Error },
    #[snafu(display("Could not deserialize config: {}", error))]
    Yaml { error: serde_yaml::Error },
    #[snafu(display("Could not deserialize prometheus response: {}", error))]
    Prometheus {
        error: prometheus_parser::ParserError,
    },
    #[snafu(display("Could not query time: {}", error))]
    Time { error: SystemTimeError },
}

impl From<SystemTimeError> for Error {
    fn from(error: SystemTimeError) -> Self {
        Self::Time { error }
    }
}

impl From<serde_yaml::Error> for Error {
    fn from(error: serde_yaml::Error) -> Self {
        Self::Yaml { error }
    }
}

impl From<prometheus_parser::ParserError> for Error {
    fn from(error: prometheus_parser::ParserError) -> Self {
        Self::Prometheus { error }
    }
}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        Self::Reqwest { error }
    }
}

impl From<http::uri::InvalidUri> for Error {
    fn from(error: http::uri::InvalidUri) -> Self {
        Self::Http { error }
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io { error }
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json { error }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum Status {
    Success,
    Error,
}

struct Worker {
    experiment_name: String,
    variant: soak::Variant,
    targets: Vec<(Target, Url)>,
    capture_path: String,
}

impl Worker {
    fn new(config: Config) -> Self {
        let mut targets = Vec::with_capacity(config.targets.len());
        for target in config.targets {
            let url = target.url.parse::<Url>().expect("failed to parse URL");
            targets.push((target, url));
        }

        Self {
            experiment_name: config.experiment_name,
            variant: config.variant,
            targets,
            capture_path: config.capture_path,
        }
    }

    async fn run(self) -> Result<(), Error> {
        let run_id: Uuid = Uuid::new_v4();
        let client: reqwest::Client = reqwest::Client::new();

        let mut wtr = fs::File::create(self.capture_path)?;
        for fetch_index in 0..u64::max_value() {
            let now_ms: u128 = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
            for (target, url) in &self.targets {
                let request = client.get(url.clone()).build()?;
                let response = match client.execute(request).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        debug!(
                            "Did not receive a response from {} with error: {}",
                            target.id, e
                        );
                        continue;
                    }
                };
                let body = response.text().await?;
                let metric_groups = prometheus_parser::parse_text(&body)?;

                if metric_groups.is_empty() {
                    error!("failed to request body: {:?}", body);
                }

                for metric in metric_groups {
                    let metric_name = metric.name;
                    let (kind, mm) = match metric.metrics {
                        GroupKind::Summary(..)
                        | GroupKind::Histogram(..)
                        | GroupKind::Untyped(..) => continue,
                        GroupKind::Counter(mm) => (soak::MetricKind::Counter, mm),
                        GroupKind::Gauge(mm) => (soak::MetricKind::Gauge, mm),
                    };
                    for (k, v) in mm.iter() {
                        let timestamp = k.timestamp.map(|x| x as u128).unwrap_or(now_ms);
                        let value = v.value;
                        let output = soak::Output {
                            run_id: Cow::Borrowed(&run_id),
                            experiment: Cow::Borrowed(&self.experiment_name),
                            variant: self.variant,
                            target: target.id.clone(),
                            time: timestamp,
                            fetch_index,
                            metric_name: metric_name.clone(),
                            metric_kind: kind,
                            metric_labels: k.labels.clone(),
                            value,
                        };
                        serde_json::to_writer(&mut wtr, &output)?;
                        wtr.write_all(b"\n")?;
                        wtr.flush()?;
                    }
                }
            }
            sleep(Duration::from_secs(1)).await;
        }
        // SAFETY: The only way to reach this point is to break the above loop
        // -- which we do not -- or to traverse u64::MAX seconds, implying that
        // the computer running this program has been migrated away from the
        // Earth meanwhile as our dear cradle is now well encompassed by the
        // sun. Unless we moved the Earth.
        unreachable!()
    }
}

fn get_config() -> Result<Config, Error> {
    let ops: Opts = argh::from_env();
    let mut file: std::fs::File = std::fs::OpenOptions::new()
        .read(true)
        .open(ops.config_path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    serde_yaml::from_str(&contents).map_err(|e| e.into())
}

fn main() -> Result<(), Error> {
    tracing_subscriber::fmt().init();
    let config: Config = get_config()?;
    debug!("CONFIG: {:?}", config);
    let runtime = Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
        .unwrap();
    let worker = Worker::new(config);
    runtime.block_on(worker.run())?;
    Ok(())
}
