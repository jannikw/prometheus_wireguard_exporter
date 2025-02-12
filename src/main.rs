use anyhow::Context;
use clap::{crate_authors, crate_name, crate_version, value_parser, Arg, ArgAction};
use hyper::{Body, Request};
use log::{debug, info, trace};
use prometheus_exporter_base::prelude::{Authorization, ServerOptions};
use std::env;
use std::{borrow::Cow, collections::HashMap};
mod options;
use options::Options;
mod wireguard;
use std::convert::TryFrom;
use std::process::Command;
mod friendly_description;
pub use friendly_description::*;
use wireguard::WireGuard;
mod exporter_error;
mod wireguard_config;
use prometheus_exporter_base::render_prometheus;
use std::net::IpAddr;
use std::sync::Arc;
use wireguard_config::{peer_entry_hashmap_try_from, PeerEntry};

async fn perform_request(
    _req: Request<Body>,
    options: Arc<Options>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let interfaces_to_handle = match &options.interfaces {
        Some(interfaces_str) => interfaces_str.clone(),
        None => vec!["all".to_owned()],
    };
    log::trace!("interfaces_to_handle == {:?}", interfaces_to_handle);

    let peer_entry_contents = options
        .extract_names_config_files
        .as_ref()
        .map(|files| {
            files // if we have values
                .iter() // for each value
                .map(|file| std::fs::read_to_string(file as &str)) // read the contents into a String
                .collect::<Result<Vec<String>, std::io::Error>>() // And transform it into a vec (stopping in case of errors)
        })
        .transpose()
        .with_context(|| "failed to read peer config file")? // bail out if there was an error
        .map(|strings| strings.join("\n")); // now join the strings in a new string

    let more_peer_names = options
        .peer_names_file
        .as_ref()
        .map(|file| std::fs::read_to_string(file).with_context(|| "failed to read peer names file"))
        .transpose()?
        .map(|cfg| serde_json::from_str::<HashMap<String, String>>(&cfg))
        .transpose()
        .with_context(|| {
            "failed to parse peer names: expected JSON object mapping public keys to names"
        })?;

    let peer_entry_hashmap = peer_entry_contents
        .as_ref()
        .map(|contents| peer_entry_hashmap_try_from(contents))
        .transpose()?;

    // Combine peer_entry_hashmap and more_peer_names into a single hashmap
    let peer_entry_hashmap = match (peer_entry_hashmap, &more_peer_names) {
        (Some(mut peer_entry_hashmap), Some(more_peer_names)) => {
            peer_entry_hashmap.extend(more_peer_names.iter().map(|(public_key, friendly_name)| {
                (
                    public_key.as_str(),
                    PeerEntry {
                        public_key,
                        allowed_ips: "",
                        friendly_description: Some(FriendlyDescription::Name(Cow::Borrowed(
                            friendly_name,
                        ))),
                    },
                )
            }));
            Some(peer_entry_hashmap)
        }
        (Some(peer_entry_hashmap), None) => Some(peer_entry_hashmap),
        (None, Some(more_peer_names)) => Some(
            more_peer_names
                .iter()
                .map(|(public_key, friendly_name)| {
                    (
                        public_key.as_str(),
                        PeerEntry {
                            public_key,
                            allowed_ips: "",
                            friendly_description: Some(FriendlyDescription::Name(Cow::Borrowed(
                                friendly_name,
                            ))),
                        },
                    )
                })
                .collect(),
        ),
        (None, None) => None,
    };

    trace!("peer_entry_hashmap == {:#?}", peer_entry_hashmap);

    let mut wg_accumulator: Option<WireGuard> = None;

    for interface_to_handle in interfaces_to_handle {
        let output = if options.prepend_sudo {
            Command::new("sudo")
                .arg("wg")
                .arg("show")
                .arg(&interface_to_handle)
                .arg("dump")
                .output()?
        } else {
            Command::new("wg")
                .arg("show")
                .arg(&interface_to_handle)
                .arg("dump")
                .output()?
        };

        let output_stdout_str = String::from_utf8(output.stdout)?;
        trace!(
            "wg show {} dump stdout == {}",
            interface_to_handle,
            output_stdout_str
        );
        let output_stderr_str = String::from_utf8(output.stderr)?;
        trace!(
            "wg show {} dump stderr == {}",
            interface_to_handle,
            output_stderr_str
        );

        // the output of wg show is different if we use all or we specify an interface.
        // In the first case the first column will be the interface name. In the second case
        // the interface name will be omitted. We need to compensate for the skew somehow (one
        // column less in the second case). We solve this prepending the interface name in every
        // line so the output of the second case will be equal to the first case.
        let output_stdout_str = if interface_to_handle != "all" {
            debug!("injecting {} to the wg show output", interface_to_handle);
            let mut result = String::new();
            for s in output_stdout_str.lines() {
                result.push_str(&format!("{}\t{}\n", interface_to_handle, s));
            }
            result
        } else {
            output_stdout_str
        };

        if let Some(wg_accumulator) = &mut wg_accumulator {
            let wg = WireGuard::try_from(&output_stdout_str as &str)?;
            wg_accumulator.merge(&wg);
        } else {
            wg_accumulator = Some(WireGuard::try_from(&output_stdout_str as &str)?);
        };
    }

    if let Some(wg_accumulator) = wg_accumulator {
        Ok(wg_accumulator.render_with_names(peer_entry_hashmap.as_ref(), &options))
    } else {
        panic!();
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let matches = clap::Command::new(crate_name!())
        .version(crate_version!())
        .author(crate_authors!("\n"))
        .arg(
            Arg::new("addr")
                .short('l')
                .long("address")
                .env("PROMETHEUS_WIREGUARD_EXPORTER_ADDRESS")
                .value_parser(value_parser!(IpAddr))
                .help("exporter address")
                .default_value("0.0.0.0")
        )
        .arg(
            Arg::new("port")
                .short('p')
                .long("port")
                .env("PROMETHEUS_WIREGUARD_EXPORTER_PORT")
                .value_parser(value_parser!(u16))
                .help("exporter port")
                .default_value("9586")
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .env("PROMETHEUS_WIREGUARD_EXPORTER_VERBOSE_ENABLED")
                .value_parser(value_parser!(bool))
                .help("verbose logging")
                .default_value("false")
        )
        .arg(
            Arg::new("prepend_sudo")
                .short('a')
                .long("prepend_sudo")
                .env("PROMETHEUS_WIREGUARD_EXPORTER_PREPEND_SUDO_ENABLED")
                .value_parser(value_parser!(bool))
                .help("Prepend sudo to the wg show commands")
                .default_value("false")
        )
        .arg(
            Arg::new("separate_allowed_ips")
                .short('s')
                .long("separate_allowed_ips")
                .env("PROMETHEUS_WIREGUARD_EXPORTER_SEPARATE_ALLOWED_IPS_ENABLED")
                .value_parser(value_parser!(bool))
                .help("separate allowed ips and ports")
                .default_value("false")
        )
        .arg(
            Arg::new("export_remote_ip_and_port")
                .short('r')
                .long("export_remote_ip_and_port")
                .env("PROMETHEUS_WIREGUARD_EXPORTER_EXPORT_REMOTE_IP_AND_PORT_ENABLED")
                .value_parser(value_parser!(bool))
                .help("exports peer's remote ip and port as labels (if available)")
                .default_value("false")
        )
        .arg(
            Arg::new("extract_names_config_files")
                .short('n')
                .long("extract_names_config_files")
                .num_args(0..)
                .env("PROMETHEUS_WIREGUARD_EXPORTER_CONFIG_FILE_NAMES")
                .help("If set, the exporter will look in the specified WireGuard config file for peer names (must be in [Peer] definition and be a comment). Multiple files are supported.")
                .use_value_delimiter(false))
        .arg(
            Arg::new("peer_names_config_file")
                .long("peer_names_config_file")
                // .num_args(0..1)
                .env("PROMETHEUS_WIREGUARD_EXPORTER_PEER_NAMES_CONFIG_FILE")
                .help("If set, the exporter will look in the specified config file for mapping peer public keys to names.")
                .action(ArgAction::Set))
        .arg(
            Arg::new("interfaces")
                .short('i')
                .long("interfaces")
                .num_args(0..)
                .env("PROMETHEUS_WIREGUARD_EXPORTER_INTERFACES")
                .help("If set specifies the interface passed to the wg show command. It is relative to the same position config_file. In not specified, all will be passed.")
                .use_value_delimiter(false))
        .arg(
            Arg::new("export_latest_handshake_delay")
                .short('d')
                .long("export_latest_handshake_delay")
                .env("EXPORT_LATEST_HANDSHAKE_DELAY")
                .value_parser(value_parser!(bool))
                .help("exports runtime calculated latest handshake delay")
                .default_value("false")
        )
         .get_matches();

    let options = Options::from_claps(&matches);

    if options.verbose {
        env::set_var(
            "RUST_LOG",
            format!("{}=trace,prometheus_exporter_base=trace", crate_name!()),
        );
    } else {
        env::set_var(
            "RUST_LOG",
            format!("{}=info,prometheus_exporter_base=info", crate_name!()),
        );
    }
    env_logger::init();

    info!(
        "{} v{} starting...",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );
    info!("using options: {:?}", options);

    let bind: u16 = *matches.get_one("port").unwrap();
    let ip: IpAddr = *matches.get_one("addr").unwrap();
    let addr: std::net::SocketAddr = (ip, bind).into();

    info!("starting exporter on http://{}/metrics", addr);

    let server_options = ServerOptions {
        addr,
        authorization: Authorization::None,
    };

    render_prometheus(server_options, options, |request, options| {
        Box::pin(perform_request(request, options))
    })
    .await;

    Ok(())
}
