use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, LastWill, MqttOptions, Outgoing, QoS};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::core::{Core, Snapshot, State, SystemEvent};
use crate::entities::{self, Entity};

const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// Capacity of the request channel. Larger than the burst of one connect, so
/// `try_publish` from the task that polls the event loop never fails for a full channel.
const CHANNEL_CAP: usize = 64;
const RETRY_DELAY: Duration = Duration::from_secs(5);
const OFFLINE_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const DISCONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// The interval at which the source gives a snapshot.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

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

/// The messages of each (re)connect: the discovery configs of the supplied
/// entities, then `online`, then the given states.
pub fn connect_messages(
    cfg: &Config,
    model: &str,
    supplied: &[&Entity],
    states: &[(&Entity, String)],
) -> Vec<Message> {
    let device = &cfg.device.name;
    let mut msgs: Vec<Message> = supplied
        .iter()
        .map(|e| Message {
            topic: entities::config_topic(device, e),
            payload: entities::discovery_payload(cfg, e, model).to_string(),
        })
        .collect();
    msgs.push(availability_message(cfg, ONLINE));
    msgs.extend(
        states
            .iter()
            .map(|(entity, value)| state_message(cfg, entity, value.clone())),
    );
    msgs
}

fn state_message(cfg: &Config, entity: &Entity, payload: String) -> Message {
    Message {
        topic: entities::state_topic(&cfg.device.name, entity),
        payload,
    }
}

fn availability_message(cfg: &Config, payload: &str) -> Message {
    Message {
        topic: entities::availability_topic(&cfg.device.name),
        payload: payload.to_string(),
    }
}

/// Handles one event of the source: its `lastsystemstatechange` state, then
/// the availability. A `Suspend` reply is completed after both are queued.
fn handle_event(
    cfg: &Config,
    core: &mut Core,
    event: SystemEvent,
    mut publish: impl FnMut(Message),
) {
    match event {
        SystemEvent::Suspend { queued } => {
            for (entity, value) in core.event("Suspend") {
                publish(state_message(cfg, entity, value));
            }
            publish(availability_message(cfg, OFFLINE));
            let _ = queued.send(());
        }
        SystemEvent::Resume => {
            for (entity, value) in core.event("Resume") {
                publish(state_message(cfg, entity, value));
            }
            publish(availability_message(cfg, ONLINE));
        }
    }
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

    fn publish_states(&mut self, cfg: &Config, states: Vec<State>) {
        for (entity, value) in states {
            self.publish(state_message(cfg, entity, value));
        }
    }

    fn record(&mut self, event: &Event) {
        if let Event::Outgoing(Outgoing::Publish(_)) = event {
            self.sent += 1;
        }
    }
}

/// Runs the agent until `stop` completes, then publishes `offline` and disconnects.
/// `model` is the device model of the discovery configs. Every 2 s, `poll`
/// gives a snapshot to `core`; `events` gives the sleep and wake events.
pub async fn run<P, F>(
    cfg: &Config,
    model: &str,
    mut core: Core,
    mut poll: P,
    mut events: mpsc::Receiver<SystemEvent>,
    stop: F,
) -> Result<()>
where
    P: FnMut() -> Snapshot,
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
    // The first snapshot is not published: the connect publishes it.
    core.update(poll(), Instant::now());
    let mut poll_tick = interval(POLL_INTERVAL);
    poll_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // While disconnected, no state is queued: the connect publishes all states.
    // The tick is off then too, because a cancel of `eventloop.poll()` drops a
    // connect in progress (its timeout is longer than the tick).
    let mut connected = false;
    // After a connection error: the time of the next connect attempt.
    let mut retry_at: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = tokio::time::sleep_until(retry_at.unwrap_or_else(tokio::time::Instant::now)),
                if retry_at.is_some() => retry_at = None,
            _ = poll_tick.tick(), if connected => {
                let states = core.update(poll(), Instant::now());
                if connected {
                    session.publish_states(cfg, states);
                }
            }
            Some(event) = events.recv() => {
                handle_event(cfg, &mut core, event, |msg| {
                    if connected {
                        session.publish(msg);
                    }
                });
            }
            event = session.eventloop.poll(), if retry_at.is_none() => match event {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    info!(host = %cfg.mqtt.host, port = cfg.mqtt.port, "connected to broker");
                    connected = true;
                    // The tick was off while disconnected: take a fresh snapshot.
                    core.update(poll(), Instant::now());
                    let states = core.connect(Instant::now());
                    for msg in connect_messages(cfg, model, core.supplied(), &states) {
                        session.publish(msg);
                    }
                }
                Ok(event) => {
                    session.record(&event);
                    debug!(?event, "mqtt event");
                }
                Err(e) => {
                    connected = false;
                    warn!(error = %e, "mqtt connection error, retry in {} s", RETRY_DELAY.as_secs());
                    retry_at = Some(tokio::time::Instant::now() + RETRY_DELAY);
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
    session.publish(availability_message(cfg, OFFLINE));

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
            "3.1.0",
            &[&entities::LASTBOOT, &entities::SESSIONSTATE],
            &[
                (&entities::LASTBOOT, "2026-08-28 04:54:55Z".to_string()),
                (&entities::SESSIONSTATE, "Unknown".to_string()),
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
        assert_eq!(config["device"]["model"], "3.1.0");
    }

    static WITH_EVENTS: [&Entity; 2] = [&entities::SESSIONSTATE, &entities::LASTSYSTEMSTATECHANGE];

    /// Sends `event` through a fake channel and gives the messages that the
    /// core publishes for it.
    fn messages_of(core: &mut Core, event: SystemEvent) -> Vec<(String, String)> {
        let cfg = Config::parse(CFG).unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(event).unwrap();
        let mut msgs = Vec::new();
        handle_event(&cfg, core, rx.try_recv().unwrap(), |m| {
            msgs.push((m.topic, m.payload))
        });
        msgs
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(t, p)| (t.to_string(), p.to_string()))
            .collect()
    }

    #[test]
    fn suspend_publishes_suspend_then_offline_and_completes_the_reply() {
        let mut core = Core::new(&WITH_EVENTS, None);
        let (queued, mut done) = tokio::sync::oneshot::channel();
        let msgs = messages_of(&mut core, SystemEvent::Suspend { queued });
        assert_eq!(
            msgs,
            pairs(&[
                (
                    "homeassistant/sensor/JAHURTX-TEST/lastsystemstatechange/state",
                    "Suspend"
                ),
                ("homeassistant/sensor/JAHURTX-TEST/availability", "offline"),
            ])
        );
        assert_eq!(done.try_recv(), Ok(()));
    }

    #[test]
    fn resume_publishes_resume_then_online() {
        let mut core = Core::new(&WITH_EVENTS, None);
        let msgs = messages_of(&mut core, SystemEvent::Resume);
        assert_eq!(
            msgs,
            pairs(&[
                (
                    "homeassistant/sensor/JAHURTX-TEST/lastsystemstatechange/state",
                    "Resume"
                ),
                ("homeassistant/sensor/JAHURTX-TEST/availability", "online"),
            ])
        );
    }

    #[test]
    fn suspend_without_the_event_entity_publishes_only_offline() {
        static NO_EVENTS: [&Entity; 1] = [&entities::SESSIONSTATE];
        let mut core = Core::new(&NO_EVENTS, None);
        let (queued, mut done) = tokio::sync::oneshot::channel();
        let msgs = messages_of(&mut core, SystemEvent::Suspend { queued });
        assert_eq!(
            msgs,
            pairs(&[("homeassistant/sensor/JAHURTX-TEST/availability", "offline")])
        );
        assert_eq!(done.try_recv(), Ok(()));
    }
}
