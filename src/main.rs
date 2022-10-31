use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    num::NonZeroU16,
    os::unix::prelude::{FromRawFd, IntoRawFd},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use clap::Parser;
use futures::{
    future::{select, Either},
    pin_mut,
    stream::try_unfold,
    Stream, StreamExt,
};
use mqtt::{Client, ClientSessionState, ConnectionSettings, QoSLevel};
use pnet::packet::icmp::{echo_request::MutableEchoRequestPacket, IcmpType};
use serde::{de::Visitor, Deserialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::{
    net::UdpSocket,
    time::{interval, Interval},
};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    let password = if let Some(password_file) = args.password_file {
        let password = tokio::fs::read(password_file)
            .await
            .context("failed to read password file")?;
        Some(Bytes::from(password))
    } else {
        None
    };

    let config = tokio::fs::read_to_string(&args.config)
        .await
        .context("failed to read config file")?;
    let config = serde_yaml::from_str::<Config>(&config).context("failed to parse config")?;

    if config.devices.is_empty() {
        bail!("no devices specified");
    }
    let send_interval = Duration::from_secs(config.send_interval);

    let socket = Socket::new(Domain::IPV4, Type::RAW, Some(Protocol::ICMPV4))?;
    socket.set_nonblocking(true)?;
    let socket =
        UdpSocket::from_std(unsafe { std::net::UdpSocket::from_raw_fd(socket.into_raw_fd()) })?;
    let socket = Arc::new(socket);

    let connection_settings = ConnectionSettings {
        keep_alive: NonZeroU16::new(3),
        user_name: args.username,
        password,
        ..ConnectionSettings::default()
    };

    let mut session_state = ClientSessionState::new();
    let mut client = Client::new(
        (config.broker_ip, config.broker_port),
        connection_settings.clone(),
        &mut session_state,
    )
    .await
    .context("failed to connect")?;

    for name in config.devices.keys() {
        client
            .subscribe(&format!("{}/command/{}", config.topic_base, name))
            .await?;
    }

    let checker = checker(
        socket.clone(),
        send_interval,
        config.timeout,
        &config.devices,
    );
    pin_mut!(checker);

    let udp_socket = UdpSocket::bind(("0.0.0.0", 0))
        .await
        .context("failed to bind udp socket")?;
    udp_socket
        .set_broadcast(true)
        .context("failed to set udp socket to broadcast")?;

    loop {
        let res = {
            let checker_next_future = checker.next();
            let client_receive_future = client.receive();

            pin_mut!(checker_next_future);
            pin_mut!(client_receive_future);

            let res = select(checker_next_future, client_receive_future).await;
            match res {
                Either::Left((res, _)) => Either::Left(res),
                Either::Right((res, _)) => Either::Right(res),
            }
        };

        match res {
            Either::Left(res) => {
                let (device, state) = res.unwrap()?;
                info!(device, state, "state changed");

                client
                    .publish(
                        format!(
                            "{topic_base}/status/{device}",
                            topic_base = config.topic_base
                        ),
                        Bytes::from_static(state.as_bytes()),
                        QoSLevel::AtMostOnce,
                    )
                    .await?;
            }
            Either::Right(res) => {
                let (topic, _) = res?;

                for (_, dev) in config.devices.iter().filter(|(name, _)| {
                    let device_topic = format!("{}/command/{}", config.topic_base, name);
                    topic == device_topic
                }) {
                    let mut buf: [u8; 102] = [0xff; 102];
                    buf[6..].chunks_mut(6).for_each(|chunk| {
                        chunk.copy_from_slice(&dev.hw_address.0);
                    });

                    udp_socket
                        .send_to(&buf, ("255.255.255.255", 9))
                        .await
                        .context("failed to send wake packet")?;
                }
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    username: Option<String>,
    #[arg(short, long)]
    password_file: Option<PathBuf>,
    config: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    broker_ip: IpAddr,
    #[serde(default = "default_port")]
    broker_port: u16,
    topic_base: String,
    send_interval: u64,
    timeout: u64,
    devices: HashMap<String, Device>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    ip: Ipv4Addr,
    hw_address: HwAddress,
}

struct HwAddress([u8; 6]);

impl<'de> Deserialize<'de> for HwAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct HwAddressVisitor;

        impl<'de> Visitor<'de> for HwAddressVisitor {
            type Value = HwAddress;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                write!(formatter, "a mac address (string)")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let (first, rest) = v.split_once(':').ok_or_else(|| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &"six separated hex numbers",
                    )
                })?;
                let (second, rest) = rest.split_once(':').ok_or_else(|| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &"six separated hex numbers",
                    )
                })?;
                let (third, rest) = rest.split_once(':').ok_or_else(|| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &"six separated hex numbers",
                    )
                })?;
                let (fourth, rest) = rest.split_once(':').ok_or_else(|| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &"six separated hex numbers",
                    )
                })?;
                let (fifth, sixth) = rest.split_once(':').ok_or_else(|| {
                    let err = serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(v),
                        &"six separated hex numbers",
                    );
                    err
                })?;

                let first = u8::from_str_radix(first, 16).map_err(|_| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(first),
                        &"one or two digit hex number",
                    )
                })?;
                let second = u8::from_str_radix(second, 16).map_err(|_| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(second),
                        &"one or two digit hex number",
                    )
                })?;
                let third = u8::from_str_radix(third, 16).map_err(|_| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(third),
                        &"one or two digit hex number",
                    )
                })?;
                let fourth = u8::from_str_radix(fourth, 16).map_err(|_| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(fourth),
                        &"one or two digit hex number",
                    )
                })?;
                let fifth = u8::from_str_radix(fifth, 16).map_err(|_| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(fifth),
                        &"one or two digit hex number",
                    )
                })?;
                let sixth = u8::from_str_radix(sixth, 16).map_err(|_| {
                    serde::de::Error::invalid_value(
                        serde::de::Unexpected::Str(sixth),
                        &"one or two digit hex number",
                    )
                })?;

                Ok(HwAddress([first, second, third, fourth, fifth, sixth]))
            }
        }

        deserializer.deserialize_str(HwAddressVisitor)
    }
}

fn default_port() -> u16 {
    1883
}

fn checker(
    socket: Arc<UdpSocket>,
    send_interval: Duration,
    timeout: u64,
    devices: &HashMap<String, Device>,
) -> impl Stream<Item = Result<(String, &'static str)>> {
    struct DeviceState {
        name: String,
        counter: u64,
    }

    struct PingPongState {
        socket: Arc<UdpSocket>,
        send_timer: Interval,
        timeout: u64,
        last_seen: HashMap<Ipv4Addr, DeviceState>,
        previous_states: HashMap<String, &'static str>,
    }

    impl PingPongState {
        async fn next(&mut self) -> Result<(String, &'static str)> {
            loop {
                let res = {
                    let recv_future = self.socket.recv_from(&mut []);
                    let tick_future = self.send_timer.tick();

                    pin_mut!(recv_future);
                    pin_mut!(tick_future);

                    let res = select(recv_future, tick_future).await;
                    match res {
                        Either::Left((res, _)) => {
                            let (_len, addr) = res?;
                            Either::Left(addr)
                        }
                        Either::Right(_) => Either::Right(()),
                    }
                };

                match res {
                    Either::Left(addr) => {
                        let addr = if let SocketAddr::V4(addr) = addr {
                            addr
                        } else {
                            continue;
                        };
                        let state = if let Some(state) = self.last_seen.get_mut(addr.ip()) {
                            state
                        } else {
                            continue;
                        };
                        state.counter = 0;
                        return Ok((state.name.clone(), "UP"));
                    }
                    Either::Right(_) => {
                        let mut buffer = [0; 8];
                        let mut packet = MutableEchoRequestPacket::new(&mut buffer).unwrap();
                        packet.set_icmp_type(IcmpType::new(8));
                        packet.set_checksum(0xf7ff);

                        for (&ip, state) in self.last_seen.iter_mut() {
                            self.socket
                                .send_to(&buffer, SocketAddr::V4(SocketAddrV4::new(ip, 0)))
                                .await?;

                            let prev = state.counter;
                            state.counter = prev + 1;

                            if prev >= self.timeout {
                                return Ok((state.name.clone(), "DOWN"));
                            }
                        }
                    }
                }
            }
        }
    }

    let last_seen = devices
        .iter()
        .map(|(name, dev)| {
            (
                dev.ip,
                DeviceState {
                    name: name.clone(),
                    counter: 0,
                },
            )
        })
        .collect();

    let state = PingPongState {
        socket,
        send_timer: interval(send_interval),
        timeout,
        last_seen,
        previous_states: HashMap::new(),
    };

    try_unfold(Box::new(state), |mut state| async move {
        loop {
            let (device, s) = state.next().await?;
            if state.previous_states.insert(device.clone(), s) != Some(s) {
                return Ok(Some(((device, s), state)));
            }
        }
    })
}
