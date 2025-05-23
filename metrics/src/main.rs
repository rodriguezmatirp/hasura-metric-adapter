use std::collections::HashMap;

use actix_web::{App, get, HttpServer, Responder};

use clap::Parser;
use clap::builder::TypedValueParser;

use regex::Regex;
use log::{info, warn, debug, error};

use prometheus::{Encoder, TextEncoder};
use tokio::sync::watch;
use crate::telemetry::Telemetry;

mod logreader;
mod logprocessor;
mod collectors;

mod telemetry;


#[get("/metrics")]
async fn metrics() -> impl Responder {
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();

    // Gather the metrics.
    let metric_families = prometheus::gather();
    // Encode them to send.
    encoder.encode(&metric_families, &mut buffer).unwrap();

    String::from_utf8(buffer.clone()).unwrap()
}

async fn webserver(cfg: &Configuration) -> std::io::Result<()> {
    warn!("Starting metric server @ {}", cfg.listen_addr);
    HttpServer::new(|| App::new().service(metrics))
        .bind(&cfg.listen_addr)?
        .run()
        .await
}

#[derive(clap::ValueEnum, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Collectors {
    CronTriggers,
    EventTriggers,
    ScheduledEvents,
    MetadataInconsistency,
}

fn key_value_parser(input: &str) -> Result<(String, String), String> {
    let pair: Vec<&str> = Regex::new(r"=").unwrap().split(&input).collect();
    match pair.len() {
        2 => Ok((String::from(pair[0]),String::from(pair[1]))),
        _ => Err(format!("invalid KEY=value: no `=` found in `{}`",input)),
    }
}

/// Implementation for [`ValueParser::string`]
///
/// Useful for composing new [`TypedValueParser`]s
#[derive(Copy, Clone, Debug)]
#[non_exhaustive]
pub struct MapValueParser {}
impl MapValueParser {
    /// Implementation for [`ValueParser::string`]
    pub fn new() -> Self {
        Self {}
    }
}

impl TypedValueParser for MapValueParser {
    type Value = HashMap<String,String>;

    fn parse_ref(
        &self,
        _cmd: &clap::Command,
        _arg: Option<&clap::Arg>,
        value: &std::ffi::OsStr,
    ) -> Result<Self::Value, clap::Error> {
        let map: HashMap<String,String> = Regex::new(r",")
            .unwrap()
            .split(value.to_str().unwrap())
            .map(|x| key_value_parser(x).unwrap())
            .collect();
        Ok(map)
    }
}

#[derive(Parser,Debug)]
#[clap(author, version, about)]
pub(crate) struct Configuration {
    #[clap(name ="listen", long = "listen", env = "LISTEN_ADDR", default_value = "0.0.0.0:9090")]
    listen_addr: String,

    #[clap(name ="hasura-endpoint", long = "hasura-endpoint", env = "HASURA_GRAPHQL_ENDPOINT", default_value = "http://localhost:8080")]
    hasura_addr: String,

    #[clap(name ="hasura-admin-secret", long = "hasura-admin-secret", env = "HASURA_GRAPHQL_ADMIN_SECRET")]
    hasura_admin: Option<String>,

    #[clap(name ="logfile", long = "logfile", env = "LOG_FILE")]
    log_file: String,

    #[clap(name ="sleep", long = "sleep", env = "SLEEP_TIME", default_value = "1000")]
    sleep_time: u64,

    #[clap(name ="collect-interval", long = "collect-interval", env = "COLLECT_INTERVAL", default_value = "15000")]
    collect_interval: u64,

    #[clap(name ="exclude-collectors", long = "exclude-collectors", env = "EXCLUDE_COLLECTORS", value_parser, value_delimiter(';'))]
    disabled_collectors: Vec<Collectors>,

    #[clap(name ="common-labels", short = 'l', long = "common-labels", env = "COMMON_LABELS", value_parser = MapValueParser::new())]
    common_labels: Option<HashMap<String,String>>,

    #[clap(name ="histogram-buckets", long = "histogram-buckets", env = "HISTOGRAM_BUCKETS", value_parser, value_delimiter(';'))]
    histogram_buckets: Vec<f64>,

    #[clap(name ="concurrency-limit", long = "concurrency-limit", env = "CONCURRENCY_LIMIT", default_value = "0")]
    concurrency_limit: usize,
}

async fn signal_handler_ctrl_c(tx: watch::Sender<()>) -> std::io::Result<()> {
    tokio::signal::ctrl_c().await?;
    warn!("Terminating due to ctrl+c");
    let _ = tx.send(());
    Ok(())
}

fn signal_handler() -> watch::Receiver<()> {
    let (terminate_tx, terminate_rx) = watch::channel(());
    tokio::spawn(signal_handler_ctrl_c(terminate_tx));
    terminate_rx
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let mut config = Configuration::parse();

    if config.hasura_admin.is_none() {
        let admin_collectors = [
            Collectors::CronTriggers,
            Collectors::EventTriggers,
            Collectors::ScheduledEvents,
            Collectors::MetadataInconsistency,
        ];

        config.disabled_collectors.extend_from_slice(&admin_collectors);

        warn!("No Hasura admin secret provided, disabling following collectors: {:?}", &admin_collectors);
    }

    config.disabled_collectors.sort();
    config.disabled_collectors.dedup();

    info!("hasura-metrics-adapter on {0} for hasura at {1} parsing hasura log '{2}'", config.listen_addr, config.hasura_addr, config.log_file);

    debug!("Configuration: {:?}", config);

    let terminate_rx = signal_handler();

    let metric_obj: Telemetry = Telemetry::new(config.common_labels.clone().unwrap_or_default(),config.histogram_buckets.clone());

    let res = tokio::try_join!(
        webserver(&config),
        logreader::read_file(&config.log_file, &metric_obj, config.sleep_time, terminate_rx.clone()),
        collectors::run_metadata_collector(&config, &metric_obj, terminate_rx.clone())
    );

    match res {
        Ok(_) => {
            info!("All tasks completed successfully");
        }
        Err(e) => {
            error!("System error: {}", e);
            return Err(e.into());
        }
    }

    Ok(())
}