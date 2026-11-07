use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, LastWill, MqttOptions, Outgoing, QoS};
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::entities::{self, SENSORS};

const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// Capacity of the request channel. Larger than the burst of one connect, so
/// `try_publish` from the task that polls the event loop never fails for a full channel.
const CHANNEL_CAP: usize = 64;
const RETRY_DELAY: Duration = Duration::from_secs(5);
const OFFLINE_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const DISCONNECT_TIMEOUT: Duration = Duration::from_millis(500);

pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";

/// One message to publish (always retained, QoS 1).
#[derive(Debug, PartialEq)]
pub struct Message {
    pub topic: String,
    pub payload: String,
}

pub fn options(cfg: &Config) -> MqttOptions {
    let device = &cfg.device.name;
    let mut opts = MqttOptions::new(
        format!("pc-agent-{device}"),
        cfg.mqtt.host.clone(),
        cfg.mqtt.port,
    );
    opts.set_keep_alive(KEEP_ALIVE);
    opts.set_credentials(cfg.mqtt.username.clone(), cfg.mqtt.password.clone());
    opts.set_last_will(LastWill::new(
        entities::availability_topic(device),
        OFFLINE,
        QoS::AtLeastOnce,
        true,
    ));
    opts
}

/// The messages of each (re)connect: all discovery configs, then `online`,
/// then the given states.
pub fn connect_messages(cfg: &Config, states: &[(&str, String)]) -> Vec<Message> {
    let device = &cfg.device.name;
    let mut msgs: Vec<Message> = SENSORS
        .iter()
        .map(|s| Message {
            topic: entities::config_topic(device, s.name),
            payload: entities::discovery_payload(cfg, s).to_string(),
        })
        .collect();
    msgs.push(Message {
        topic: entities::availability_topic(device),
        payload: ONLINE.to_string(),
    });
    msgs.extend(states.iter().map(|(name, value)| Message {
        topic: entities::state_topic(device, name),
        payload: value.clone(),
    }));
    msgs
}

/// The MQTT connection: the client, its event loop, and counters that let
/// the shutdown find the packet id of the `offline` publish.
struct Session {
    client: AsyncClient,
    eventloop: EventLoop,
    /// Publishes handed to the request channel.
    queued: usize,
    /// Publishes written to the network (`Outgoing::Publish`).
    sent: usize,
}

impl Session {
    fn publish(&mut self, msg: Message) {
        match self
            .client
            .try_publish(&msg.topic, QoS::AtLeastOnce, true, msg.payload)
        {
            Ok(()) => self.queued += 1,
            Err(e) => warn!(topic = %msg.topic, error = %e, "cannot queue publish"),
        }
    }

    fn record(&mut self, event: &Event) {
        if let Event::Outgoing(Outgoing::Publish(_)) = event {
            self.sent += 1;
        }
    }
}

/// Runs the agent until `stop` completes, then publishes `offline` and disconnects.
/// `states` gives the current states at each connect.
pub async fn run<S, F>(cfg: &Config, states: S, stop: F) -> Result<()>
where
    S: Fn() -> Vec<(&'static str, String)>,
    F: Future<Output = ()>,
{
    let (client, eventloop) = AsyncClient::new(options(cfg), CHANNEL_CAP);
    let mut session = Session {
        client,
        eventloop,
        queued: 0,
        sent: 0,
    };
    tokio::pin!(stop);

    loop {
        tokio::select! {
            _ = &mut stop => break,
            event = session.eventloop.poll() => match event {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    info!(host = %cfg.mqtt.host, port = cfg.mqtt.port, "connected to broker");
                    for msg in connect_messages(cfg, &states()) {
                        session.publish(msg);
                    }
                }
                Ok(event) => {
                    session.record(&event);
                    debug!(?event, "mqtt event");
                }
                Err(e) => {
                    warn!(error = %e, "mqtt connection error, retry in {} s", RETRY_DELAY.as_secs());
                    tokio::select! {
                        _ = &mut stop => break,
                        _ = sleep(RETRY_DELAY) => {}
                    }
                }
            }
        }
    }

    shutdown(&mut session, cfg).await;
    Ok(())
}

async fn shutdown(session: &mut Session, cfg: &Config) {
    info!("stopping: publish offline and disconnect");
    let before = session.queued - session.sent;
    session.publish(Message {
        topic: entities::availability_topic(&cfg.device.name),
        payload: OFFLINE.to_string(),
    });

    let acked = timeout(OFFLINE_ACK_TIMEOUT, async {
        // The offline publish is the (before + 1)th publish still to be written.
        let mut to_skip = before;
        let mut offline_pkid = None;
        loop {
            match session.eventloop.poll().await {
                Ok(Event::Outgoing(Outgoing::Publish(pkid))) if offline_pkid.is_none() => {
                    if to_skip == 0 {
                        offline_pkid = Some(pkid);
                    } else {
                        to_skip -= 1;
                    }
                }
                Ok(Event::Incoming(Incoming::PubAck(ack))) if Some(ack.pkid) == offline_pkid => {
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "mqtt connection error while stopping");
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    })
    .await;
    match acked {
        Ok(()) => info!("offline acknowledged"),
        Err(_) => warn!(
            "no ack for offline within {} s",
            OFFLINE_ACK_TIMEOUT.as_secs()
        ),
    }

    if let Err(e) = session.client.try_disconnect() {
        warn!(error = %e, "cannot queue disconnect");
        return;
    }
    let _ = timeout(DISCONNECT_TIMEOUT, async {
        loop {
            match session.eventloop.poll().await {
                Ok(Event::Outgoing(Outgoing::Disconnect)) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[mqtt]
host = "broker.example"
port = 1884
username = "u"
password = "p"

[device]
name = "JAHURTX-TEST"
identifier = "pc-agent-JAHURTX-TEST"
entity_prefix = "jahurtx_test"

[gpu]
enabled = false
"#;

    #[test]
    fn options_have_client_id_keep_alive_and_last_will() {
        let cfg = Config::parse(CFG).unwrap();
        let opts = options(&cfg);
        assert_eq!(opts.client_id(), "pc-agent-JAHURTX-TEST");
        assert_eq!(opts.broker_address(), ("broker.example".to_string(), 1884));
        assert_eq!(opts.keep_alive(), Duration::from_secs(30));
        let will = opts.last_will().unwrap();
        assert_eq!(will.topic, "homeassistant/sensor/JAHURTX-TEST/availability");
        assert_eq!(&will.message[..], b"offline");
        assert_eq!(will.qos, QoS::AtLeastOnce);
        assert!(will.retain);
    }

    #[test]
    fn connect_messages_are_configs_then_online_then_states() {
        let cfg = Config::parse(CFG).unwrap();
        let msgs = connect_messages(
            &cfg,
            &[
                ("lastboot", "2026-08-28 04:54:55Z".to_string()),
                ("sessionstate", "Unknown".to_string()),
            ],
        );
        let topics: Vec<&str> = msgs.iter().map(|m| m.topic.as_str()).collect();
        assert_eq!(
            topics,
            [
                "homeassistant/sensor/JAHURTX-TEST/lastboot/config",
                "homeassistant/sensor/JAHURTX-TEST/sessionstate/config",
                "homeassistant/sensor/JAHURTX-TEST/availability",
                "homeassistant/sensor/JAHURTX-TEST/lastboot/state",
                "homeassistant/sensor/JAHURTX-TEST/sessionstate/state",
            ]
        );
        assert_eq!(msgs[2].payload, "online");
        assert_eq!(msgs[3].payload, "2026-08-28 04:54:55Z");
        assert_eq!(msgs[4].payload, "Unknown");
        let config: serde_json::Value = serde_json::from_str(&msgs[0].payload).unwrap();
        assert_eq!(config["name"], "lastboot");
    }
}
