use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::Result;
use rumqttc::{AsyncClient, Event, EventLoop, Incoming, LastWill, MqttOptions, Outgoing, QoS};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::core::{Core, Snapshot, State, SystemEvent};
use crate::entities::{self, Entity};
use crate::update::{Step, Updater, Watch};

const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// Capacity of the request channel. Larger than the burst of one connect, so
/// `try_publish` from the task that polls the event loop never fails for a full channel.
const CHANNEL_CAP: usize = 64;
const RETRY_DELAY: Duration = Duration::from_secs(5);
const DISCONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// The interval at which the source gives a snapshot.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";

/// Why the run loop ended.
#[derive(Debug, PartialEq)]
pub enum Exit {
    /// `stop` completed: the agent published `offline` and disconnected.
    Stopped,
    /// The service must start again (the rollback of an update, or the
    /// install of an update): a clean DISCONNECT with no `offline`, so
    /// HA keeps `online`, and the caller exits so that the service manager
    /// starts the binary at the binary path.
    Restart,
}

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

/// The options of the MQTT check of `pc-agent diagnose`.
#[cfg(any(windows, test))]
pub fn diagnose_options(cfg: &Config) -> MqttOptions {
    // Its own client ID, so the broker does not end the session of the
    // agent, and no last will, so no `offline` is left behind.
    let mut opts = MqttOptions::new(
        format!("pc-agent-diagnose-{}", cfg.device.name),
        cfg.mqtt.host.clone(),
        cfg.mqtt.port,
    );
    opts.set_keep_alive(KEEP_ALIVE);
    opts.set_credentials(cfg.mqtt.username.clone(), cfg.mqtt.password.clone());
    opts
}

/// The messages of each (re)connect: the discovery configs of the supplied
/// entities and of the `update` entity (every OS supplies it), an empty
/// (retained) config for each entity of the table that is not supplied, so
/// HA removes a stale entity, then `online`, then the given states and the
/// state of the `update` entity. Outside Windows, a Windows-only entity gets
/// no empty config.
pub fn connect_messages(
    cfg: &Config,
    model: &str,
    supplied: &[&Entity],
    states: &[(&Entity, String)],
    update_state: String,
) -> Vec<Message> {
    let device = &cfg.device.name;
    let mut msgs: Vec<Message> = supplied
        .iter()
        .map(|e| Message {
            topic: entities::config_topic(device, e),
            payload: entities::discovery_payload(cfg, e, model).to_string(),
        })
        .collect();
    msgs.push(Message {
        topic: entities::update_config_topic(device),
        payload: entities::update_discovery_payload(cfg, model).to_string(),
    });
    msgs.extend(
        entities::ENTITIES
            .iter()
            .filter(|e| !supplied.iter().any(|s| s.name == e.name))
            .filter(|e| cfg!(windows) || !e.windows_only)
            .map(|e| Message {
                topic: entities::config_topic(device, e),
                payload: String::new(),
            }),
    );
    msgs.push(availability_message(cfg, ONLINE));
    msgs.extend(
        states
            .iter()
            .map(|(entity, value)| state_message(cfg, entity, value.clone())),
    );
    msgs.push(update_state_message(cfg, update_state));
    msgs
}

fn update_state_message(cfg: &Config, payload: String) -> Message {
    Message {
        topic: entities::update_state_topic(&cfg.device.name),
        payload,
    }
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
/// the availability. For `Suspend` and `Shutdown`, `offline` is the last
/// publish, and the reply comes back to the caller, which completes it after
/// the ack wait.
fn handle_event(
    cfg: &Config,
    core: &mut Core,
    event: SystemEvent,
    mut publish: impl FnMut(Message),
) -> Option<oneshot::Sender<()>> {
    match event {
        SystemEvent::Suspend { done } => {
            for (entity, value) in core.event("Suspend") {
                publish(state_message(cfg, entity, value));
            }
            publish(availability_message(cfg, OFFLINE));
            Some(done)
        }
        SystemEvent::Resume => {
            for (entity, value) in core.event("Resume") {
                publish(state_message(cfg, entity, value));
            }
            publish(availability_message(cfg, ONLINE));
            None
        }
        SystemEvent::Shutdown { done } => {
            for (entity, value) in core.event("SystemShutdown") {
                publish(state_message(cfg, entity, value));
            }
            publish(availability_message(cfg, OFFLINE));
            Some(done)
        }
        SystemEvent::ShutdownCancelled => {
            publish(availability_message(cfg, ONLINE));
            None
        }
        // The run loop takes the snapshot of a session change.
        SystemEvent::SessionChange => None,
    }
}

/// When the 2 s tick may publish states. Between `Suspend` and `Resume` the
/// tick is off: the OS locks the screen as it goes to sleep, and a
/// SessionLock must not replace the Suspend state; a connect also ends it, in
/// case the Resume never comes. After `Shutdown` the tick stays off, also
/// after a connect, until `ShutdownCancelled`.
#[derive(Debug, Default)]
struct Pause {
    asleep: bool,
    halting: bool,
}

impl Pause {
    fn on_event(&mut self, event: &SystemEvent) {
        match event {
            SystemEvent::Suspend { .. } => self.asleep = true,
            SystemEvent::Resume => self.asleep = false,
            SystemEvent::Shutdown { .. } => self.halting = true,
            SystemEvent::ShutdownCancelled => self.halting = false,
            SystemEvent::SessionChange => {}
        }
    }

    fn on_connect(&mut self) {
        self.asleep = false;
    }

    fn tick_allowed(&self) -> bool {
        !self.asleep && !self.halting
    }
}

/// Finds the broker's ack of one `offline` publish in the events of the
/// event loop.
struct AckWait {
    /// The publishes still to be written before `offline`.
    to_skip: usize,
    /// The packet id of `offline`, when it was written.
    pkid: Option<u16>,
}

impl AckWait {
    /// `backlog`: the publishes queued before `offline` and not yet written.
    fn new(backlog: usize) -> Self {
        AckWait {
            to_skip: backlog,
            pkid: None,
        }
    }

    /// Takes one event; `true` when it is the ack of `offline`.
    fn on_event(&mut self, event: &Event) -> bool {
        match event {
            Event::Outgoing(Outgoing::Publish(pkid)) if self.pkid.is_none() => {
                if self.to_skip == 0 {
                    self.pkid = Some(*pkid);
                } else {
                    self.to_skip -= 1;
                }
                false
            }
            Event::Incoming(Incoming::PubAck(ack)) => Some(ack.pkid) == self.pkid,
            _ => false,
        }
    }
}

/// The reply of a `Suspend` or `Shutdown` that waits for the ack of its `offline`.
/// The run loop keeps polling the event loop during the wait.
struct SuspendWait {
    pending: Option<PendingSuspend>,
    /// The longest wait for the ack.
    ack_wait: Duration,
}

struct PendingSuspend {
    ack: AckWait,
    deadline: tokio::time::Instant,
    done: oneshot::Sender<()>,
}

impl SuspendWait {
    fn new(ack_wait: Duration) -> Self {
        SuspendWait {
            pending: None,
            ack_wait,
        }
    }

    /// Starts the wait of `done`. `backlog` is `None` when `offline` was not
    /// queued (not connected): `done` completes at once.
    fn start(
        &mut self,
        done: oneshot::Sender<()>,
        backlog: Option<usize>,
        now: tokio::time::Instant,
    ) {
        // A second Suspend ends the wait of the first.
        self.finish();
        match backlog {
            None => {
                let _ = done.send(());
            }
            Some(backlog) => {
                self.pending = Some(PendingSuspend {
                    ack: AckWait::new(backlog),
                    deadline: now + self.ack_wait,
                    done,
                })
            }
        }
    }

    fn on_event(&mut self, event: &Event) {
        if let Some(pending) = &mut self.pending
            && pending.ack.on_event(event)
        {
            info!("offline acknowledged before sleep or shutdown");
            self.finish();
        }
    }

    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.pending.as_ref().map(|p| p.deadline)
    }

    /// Completes the reply when the deadline has passed.
    fn expire(&mut self, now: tokio::time::Instant) {
        if self.deadline().is_some_and(|d| now >= d) {
            warn!(
                "no ack for offline before sleep or shutdown within {:?}",
                self.ack_wait
            );
            self.finish();
        }
    }

    /// Completes the reply at once, if a wait is in progress.
    fn finish(&mut self) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.done.send(());
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
    /// Queues one publish; `true` when it is queued.
    fn publish(&mut self, msg: Message) -> bool {
        match self
            .client
            .try_publish(&msg.topic, QoS::AtLeastOnce, true, msg.payload)
        {
            Ok(()) => {
                self.queued += 1;
                true
            }
            Err(e) => {
                warn!(topic = %msg.topic, error = %e, "cannot queue publish");
                false
            }
        }
    }

    /// The queued publishes that are not written yet. The counters restart at
    /// each ConnAck, because a clean-session reconnect drops the unsent queue.
    fn backlog(&self) -> usize {
        self.queued.saturating_sub(self.sent)
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
/// gives a snapshot to `core`; `events` gives the sleep, wake and session
/// events. `ack_wait` is the longest wait for the ack of `offline`. The
/// first ConnAck confirms a pending update of `watch`; with no ConnAck
/// before its deadline, `watch` rolls back and the loop ends with a restart.
/// While connected, `updater` checks for updates on its schedule and on the
/// install command; after an install the loop ends with a restart.
#[allow(clippy::too_many_arguments)]
pub async fn run<P, F>(
    cfg: &Config,
    model: &str,
    mut core: Core,
    mut poll: P,
    mut events: mpsc::Receiver<SystemEvent>,
    ack_wait: Duration,
    stop: F,
    watch: &mut Watch,
    mut updater: Updater,
) -> Result<Exit>
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
    let mut suspend = SuspendWait::new(ack_wait);
    let mut pause = Pause::default();
    let command_topic = entities::update_command_topic(&cfg.device.name);

    let exit = loop {
        // Before the select: the branch of the update job borrows `updater`.
        let check_at = updater.next_check();
        tokio::select! {
            _ = &mut stop => break Exit::Stopped,
            _ = tokio::time::sleep_until(watch.deadline().unwrap_or_else(tokio::time::Instant::now)),
                if watch.deadline().is_some() => {
                if watch.on_deadline() {
                    break Exit::Restart;
                }
            }
            _ = tokio::time::sleep_until(retry_at.unwrap_or_else(tokio::time::Instant::now)),
                if retry_at.is_some() => retry_at = None,
            // A check that is due while disconnected runs after the next ConnAck.
            _ = tokio::time::sleep_until(check_at.unwrap_or_else(tokio::time::Instant::now)),
                if connected && check_at.is_some() => updater.on_timer(tokio::time::Instant::now(), chrono::Utc::now()),
            done = updater.job_done(), if updater.busy() => {
                let step = updater.on_job_done(done, chrono::Utc::now());
                if connected {
                    session.publish(update_state_message(cfg, updater.state_payload()));
                }
                if step == Step::Restart {
                    break Exit::Restart;
                }
            }
            _ = poll_tick.tick(), if connected && pause.tick_allowed() => {
                let states = core.update(poll(), Instant::now());
                if connected {
                    session.publish_states(cfg, states);
                }
            }
            Some(event) = events.recv() => {
                if let SystemEvent::SessionChange = event {
                    // The snapshot of the tick, at once; the core derives the
                    // session events from it.
                    if connected && pause.tick_allowed() {
                        let states = core.update(poll(), Instant::now());
                        session.publish_states(cfg, states);
                    }
                    continue;
                }
                pause.on_event(&event);
                // `true` when the last publish (`offline` of a Suspend) is queued.
                let mut last_queued = false;
                let done = handle_event(cfg, &mut core, event, |msg| {
                    last_queued = connected && session.publish(msg);
                });
                if let Some(done) = done {
                    // `offline` is the last queued publish: the others before it.
                    let backlog = last_queued.then(|| session.backlog().saturating_sub(1));
                    suspend.start(done, backlog, tokio::time::Instant::now());
                }
            }
            _ = tokio::time::sleep_until(suspend.deadline().unwrap_or_else(tokio::time::Instant::now)),
                if suspend.deadline().is_some() => suspend.expire(tokio::time::Instant::now()),
            event = session.eventloop.poll(), if retry_at.is_none() => match event {
                Ok(Event::Incoming(Incoming::ConnAck(_))) => {
                    info!(host = %cfg.mqtt.host, port = cfg.mqtt.port, "connected to broker");
                    connected = true;
                    watch.connected();
                    pause.on_connect();
                    // rumqttc clears its pending queue at a clean-session connect.
                    session.queued = 0;
                    session.sent = 0;
                    // The tick was off while disconnected: take a fresh snapshot.
                    core.update(poll(), Instant::now());
                    let states = core.connect(Instant::now());
                    let update_state = updater.state_payload();
                    for msg in connect_messages(cfg, model, core.supplied(), &states, update_state) {
                        session.publish(msg);
                    }
                    // A clean session: subscribe at each connect.
                    if let Err(e) = session.client.try_subscribe(&command_topic, QoS::AtLeastOnce) {
                        warn!(topic = %command_topic, error = %e, "cannot subscribe");
                    }
                }
                Ok(Event::Incoming(Incoming::Publish(p))) if p.topic == command_topic => {
                    // A retained command is old (the broker sends it at the
                    // subscribe): only a live `install` checks.
                    updater.on_command(&p.payload, p.retain);
                }
                Ok(event) => {
                    session.record(&event);
                    suspend.on_event(&event);
                    debug!(?event, "mqtt event");
                }
                Err(e) => {
                    connected = false;
                    // The ack cannot come now: the Suspend does not wait.
                    suspend.finish();
                    warn!(error = %e, "mqtt connection error, retry in {} s", RETRY_DELAY.as_secs());
                    retry_at = Some(tokio::time::Instant::now() + RETRY_DELAY);
                }
            }
        }
    };

    suspend.finish();
    match exit {
        Exit::Stopped => shutdown(&mut session, cfg, ack_wait).await,
        Exit::Restart => {
            info!("restarting: disconnect with no offline");
            disconnect(&mut session).await;
        }
    }
    Ok(exit)
}

async fn shutdown(session: &mut Session, cfg: &Config, ack_wait: Duration) {
    info!("stopping: publish offline and disconnect");
    let mut ack = AckWait::new(session.backlog());
    session.publish(availability_message(cfg, OFFLINE));

    let acked = timeout(ack_wait, async {
        loop {
            match session.eventloop.poll().await {
                Ok(event) if ack.on_event(&event) => return,
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
        Err(_) => warn!("no ack for offline within {ack_wait:?}"),
    }
    disconnect(session).await;
}

/// A clean DISCONNECT: the broker sends no last will.
async fn disconnect(session: &mut Session) {
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
    fn the_diagnose_options_have_their_own_client_id_and_no_last_will() {
        let cfg = Config::parse(CFG).unwrap();
        let opts = diagnose_options(&cfg);
        assert_eq!(opts.client_id(), "pc-agent-diagnose-JAHURTX-TEST");
        assert_ne!(opts.client_id(), options(&cfg).client_id());
        assert_eq!(opts.broker_address(), ("broker.example".to_string(), 1884));
        assert!(opts.last_will().is_none());
        assert!(opts.clean_session());
        assert_eq!(opts.credentials(), options(&cfg).credentials());
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
            "{\"installed_version\":\"0.3.0\"}".to_string(),
        );
        let topics: Vec<&str> = msgs.iter().map(|m| m.topic.as_str()).collect();
        let mut expected = vec![
            "homeassistant/sensor/JAHURTX-TEST/lastboot/config",
            "homeassistant/sensor/JAHURTX-TEST/sessionstate/config",
            // Every OS supplies the update entity.
            "homeassistant/update/JAHURTX-TEST/agent/config",
            // The entities of the table that are not supplied, in table order.
            "homeassistant/sensor/JAHURTX-TEST/lastactive/config",
            "homeassistant/sensor/JAHURTX-TEST/lastsystemstatechange/config",
            "homeassistant/sensor/JAHURTX-TEST/gpuload/config",
            "homeassistant/sensor/JAHURTX-TEST/gpupower/config",
            "homeassistant/sensor/JAHURTX-TEST/loggedusers/config",
            "homeassistant/binary_sensor/JAHURTX-TEST/display/config",
        ];
        // Only the Windows agent clears the Windows-only entity.
        if cfg!(windows) {
            expected.push("homeassistant/binary_sensor/JAHURTX-TEST/inputhelper/config");
        }
        let empty = expected.len();
        expected.extend([
            "homeassistant/sensor/JAHURTX-TEST/availability",
            "homeassistant/sensor/JAHURTX-TEST/lastboot/state",
            "homeassistant/sensor/JAHURTX-TEST/sessionstate/state",
            "homeassistant/update/JAHURTX-TEST/agent/state",
        ]);
        assert_eq!(topics, expected);
        for m in &msgs[3..empty] {
            assert_eq!(m.payload, "", "{}", m.topic);
        }
        assert_eq!(msgs[empty].payload, "online");
        assert_eq!(msgs[empty + 1].payload, "2026-08-28 04:54:55Z");
        assert_eq!(msgs[empty + 2].payload, "Unknown");
        assert_eq!(msgs[empty + 3].payload, "{\"installed_version\":\"0.3.0\"}");
        let config: serde_json::Value = serde_json::from_str(&msgs[0].payload).unwrap();
        assert_eq!(config["name"], "lastboot");
        assert_eq!(config["device"]["model"], "3.1.0");
        let update: serde_json::Value = serde_json::from_str(&msgs[2].payload).unwrap();
        assert_eq!(update["device_class"], "firmware");
        assert_eq!(update["device"]["model"], "3.1.0");
    }

    #[test]
    fn all_entities_supplied_gives_no_empty_config() {
        let cfg = Config::parse(CFG).unwrap();
        let all: Vec<&Entity> = entities::ENTITIES.iter().collect();
        let msgs = connect_messages(&cfg, "3.1.0", &all, &[], "{}".to_string());
        // The configs, the update config, online and the update state.
        assert_eq!(msgs.len(), entities::ENTITIES.len() + 3);
        assert!(msgs.iter().all(|m| !m.payload.is_empty()));
    }

    #[test]
    fn empty_configs_stay_in_the_config_topics_of_this_device() {
        let cfg = Config::parse(CFG).unwrap();
        let msgs = connect_messages(&cfg, "3.1.0", &[], &[], "{}".to_string());
        let empty: Vec<&Message> = msgs.iter().filter(|m| m.payload.is_empty()).collect();
        let clearable = entities::ENTITIES
            .iter()
            .filter(|e| cfg!(windows) || !e.windows_only)
            .count();
        assert_eq!(empty.len(), clearable);
        for m in empty {
            let t = &m.topic;
            assert!(
                (t.starts_with("homeassistant/sensor/JAHURTX-TEST/")
                    || t.starts_with("homeassistant/binary_sensor/JAHURTX-TEST/"))
                    && t.ends_with("/config"),
                "{t}"
            );
            assert!(!t.contains("hass.agent") && !t.contains("button"), "{t}");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn a_non_windows_agent_publishes_no_empty_config_for_inputhelper() {
        // The source of this OS (Linux in the CI job `linux`), with and without the GPU.
        let cfg = Config::parse(CFG).unwrap();
        for gpu in [true, false] {
            let supplied = crate::sources::supplied(gpu);
            assert!(!supplied.iter().any(|e| e.windows_only));
            let msgs = connect_messages(&cfg, "3.1.0", supplied, &[], "{}".to_string());
            assert!(
                !msgs.iter().any(|m| m.topic.contains("/inputhelper/")),
                "{:?}",
                msgs.iter().map(|m| &m.topic).collect::<Vec<_>>()
            );
        }
    }

    static WITH_EVENTS: [&Entity; 2] = [&entities::SESSIONSTATE, &entities::LASTSYSTEMSTATECHANGE];

    /// Sends `event` through a fake channel and gives the messages that the
    /// core publishes for it.
    fn messages_of(
        core: &mut Core,
        event: SystemEvent,
    ) -> (Vec<(String, String)>, Option<oneshot::Sender<()>>) {
        let cfg = Config::parse(CFG).unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(event).unwrap();
        let mut msgs = Vec::new();
        let done = handle_event(&cfg, core, rx.try_recv().unwrap(), |m| {
            msgs.push((m.topic, m.payload))
        });
        (msgs, done)
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(t, p)| (t.to_string(), p.to_string()))
            .collect()
    }

    #[test]
    fn suspend_publishes_suspend_then_offline_and_gives_back_the_reply() {
        let mut core = Core::new(&WITH_EVENTS, None);
        let (done, mut rx) = oneshot::channel();
        let (msgs, reply) = messages_of(&mut core, SystemEvent::Suspend { done });
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
        // The run loop completes the reply after the ack wait, not here.
        assert!(rx.try_recv().is_err());
        assert!(reply.is_some());
    }

    #[test]
    fn resume_publishes_resume_then_online() {
        let mut core = Core::new(&WITH_EVENTS, None);
        let (msgs, reply) = messages_of(&mut core, SystemEvent::Resume);
        assert!(reply.is_none());
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
    fn shutdown_publishes_systemshutdown_then_offline_and_gives_back_the_reply() {
        let mut core = Core::new(&WITH_EVENTS, None);
        let (done, mut rx) = oneshot::channel();
        let (msgs, reply) = messages_of(&mut core, SystemEvent::Shutdown { done });
        assert_eq!(
            msgs,
            pairs(&[
                (
                    "homeassistant/sensor/JAHURTX-TEST/lastsystemstatechange/state",
                    "SystemShutdown"
                ),
                ("homeassistant/sensor/JAHURTX-TEST/availability", "offline"),
            ])
        );
        // The run loop completes the reply after the ack wait (SuspendWait).
        assert!(rx.try_recv().is_err());
        assert!(reply.is_some());
    }

    #[test]
    fn a_cancelled_shutdown_publishes_only_online() {
        let mut core = Core::new(&WITH_EVENTS, None);
        let (msgs, reply) = messages_of(&mut core, SystemEvent::ShutdownCancelled);
        assert!(reply.is_none());
        assert_eq!(
            msgs,
            pairs(&[("homeassistant/sensor/JAHURTX-TEST/availability", "online")])
        );
    }

    #[test]
    fn the_tick_is_off_after_shutdown_also_after_a_connect() {
        let mut pause = Pause::default();
        assert!(pause.tick_allowed());
        let (done, _rx) = oneshot::channel();
        pause.on_event(&SystemEvent::Shutdown { done });
        assert!(!pause.tick_allowed());
        pause.on_connect();
        assert!(!pause.tick_allowed());
        pause.on_event(&SystemEvent::ShutdownCancelled);
        assert!(pause.tick_allowed());
    }

    #[test]
    fn the_tick_is_off_between_suspend_and_resume_or_a_connect() {
        let mut pause = Pause::default();
        let (done, _rx) = oneshot::channel();
        pause.on_event(&SystemEvent::Suspend { done });
        assert!(!pause.tick_allowed());
        pause.on_event(&SystemEvent::Resume);
        assert!(pause.tick_allowed());
        let (done, _rx) = oneshot::channel();
        pause.on_event(&SystemEvent::Suspend { done });
        pause.on_connect();
        assert!(pause.tick_allowed());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_done_completes_on_the_ack_after_2_s_or_at_once() {
        // The Shutdown reply goes through the same wait as the Suspend reply.
        let mut core = Core::new(&WITH_EVENTS, None);
        let mut wait = SuspendWait::new(Duration::from_secs(2));

        let (done, mut rx) = oneshot::channel();
        let (_, reply) = messages_of(&mut core, SystemEvent::Shutdown { done });
        wait.start(reply.unwrap(), Some(0), tokio::time::Instant::now());
        wait.on_event(&outgoing(4));
        wait.on_event(&puback(4));
        assert_eq!(rx.try_recv(), Ok(()));

        let (done, mut rx) = oneshot::channel();
        let (_, reply) = messages_of(&mut core, SystemEvent::Shutdown { done });
        wait.start(reply.unwrap(), Some(0), tokio::time::Instant::now());
        tokio::time::advance(Duration::from_secs(2)).await;
        wait.expire(tokio::time::Instant::now());
        assert_eq!(rx.try_recv(), Ok(()));

        let (done, mut rx) = oneshot::channel();
        let (_, reply) = messages_of(&mut core, SystemEvent::Shutdown { done });
        wait.start(reply.unwrap(), None, tokio::time::Instant::now());
        assert_eq!(rx.try_recv(), Ok(()));
    }

    #[test]
    fn suspend_without_the_event_entity_publishes_only_offline() {
        static NO_EVENTS: [&Entity; 1] = [&entities::SESSIONSTATE];
        let mut core = Core::new(&NO_EVENTS, None);
        let (done, mut rx) = oneshot::channel();
        let (msgs, reply) = messages_of(&mut core, SystemEvent::Suspend { done });
        assert_eq!(
            msgs,
            pairs(&[("homeassistant/sensor/JAHURTX-TEST/availability", "offline")])
        );
        // The run loop completes the reply after the ack wait, not here.
        assert!(rx.try_recv().is_err());
        assert!(reply.is_some());
    }

    fn outgoing(pkid: u16) -> Event {
        Event::Outgoing(Outgoing::Publish(pkid))
    }

    fn puback(pkid: u16) -> Event {
        Event::Incoming(Incoming::PubAck(rumqttc::PubAck::new(pkid)))
    }

    #[test]
    fn ack_wait_matches_the_ack_of_the_offline_publish_only() {
        // Two publishes were still to be written before `offline`.
        let mut wait = AckWait::new(2);
        assert!(!wait.on_event(&outgoing(7)));
        assert!(!wait.on_event(&outgoing(8)));
        assert!(!wait.on_event(&puback(7)));
        assert!(!wait.on_event(&outgoing(9)));
        assert!(!wait.on_event(&outgoing(10)));
        assert!(!wait.on_event(&puback(8)));
        assert!(!wait.on_event(&puback(10)));
        assert!(wait.on_event(&puback(9)));
    }

    #[tokio::test(start_paused = true)]
    async fn suspend_done_completes_when_offline_is_acknowledged() {
        let mut suspend = SuspendWait::new(Duration::from_secs(2));
        let (done, mut rx) = tokio::sync::oneshot::channel();
        suspend.start(done, Some(0), tokio::time::Instant::now());
        suspend.on_event(&outgoing(3));
        assert!(rx.try_recv().is_err());
        suspend.on_event(&puback(3));
        assert_eq!(rx.try_recv(), Ok(()));
        assert_eq!(suspend.deadline(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn suspend_done_completes_after_the_ack_wait_without_an_ack() {
        // 1.5 s on Windows, 2 s on Linux and macOS.
        for wait in [Duration::from_millis(1500), Duration::from_secs(2)] {
            let mut suspend = SuspendWait::new(wait);
            let (done, mut rx) = tokio::sync::oneshot::channel();
            let start = tokio::time::Instant::now();
            suspend.start(done, Some(0), start);
            assert_eq!(suspend.deadline(), Some(start + wait), "{wait:?}");
            suspend.on_event(&outgoing(3));
            tokio::time::advance(wait - Duration::from_millis(1)).await;
            suspend.expire(tokio::time::Instant::now());
            assert!(rx.try_recv().is_err(), "{wait:?}");
            tokio::time::advance(Duration::from_millis(1)).await;
            suspend.expire(tokio::time::Instant::now());
            assert_eq!(rx.try_recv(), Ok(()), "{wait:?}");
            assert_eq!(suspend.deadline(), None);
        }
    }

    #[test]
    fn the_ack_wait_is_1_5_s_on_windows_and_2_s_on_linux_and_macos() {
        let expected = if cfg!(windows) {
            Duration::from_millis(1500)
        } else {
            Duration::from_secs(2)
        };
        assert_eq!(crate::sources::ACK_WAIT, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn suspend_done_completes_at_once_when_not_connected() {
        let mut suspend = SuspendWait::new(Duration::from_secs(2));
        let (done, mut rx) = tokio::sync::oneshot::channel();
        suspend.start(done, None, tokio::time::Instant::now());
        assert_eq!(rx.try_recv(), Ok(()));
        assert_eq!(suspend.deadline(), None);
    }

    /// With no broker, the run loop ends at the deadline of the update
    /// watch with a restart, after the rollback.
    #[tokio::test(start_paused = true)]
    async fn the_rollback_deadline_ends_the_run_with_a_restart() {
        use crate::update::pending::{BuildId, Paths, Pending, Start, Watch, write_pending};
        // A port that nothing listens on.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let text = CFG
            .replace("broker.example", "127.0.0.1")
            .replace("1884", &port.to_string());
        let cfg = Config::parse(&text).unwrap();
        let root = std::env::temp_dir().join(format!("pc-agent-mqtt-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let paths = Paths {
            state_dir: root.clone(),
            exe: root.join(if cfg!(windows) {
                "pc-agent.exe"
            } else {
                "pc-agent"
            }),
        };
        std::fs::write(&paths.exe, "new build").unwrap();
        std::fs::write(paths.prev(), "old build").unwrap();
        let build = |commit: &str| BuildId {
            version: "0.3.0".to_string(),
            commit: commit.to_string(),
        };
        write_pending(&paths, &Pending::new(build("old"), build("new"))).unwrap();
        let now = chrono::Utc::now();
        let start = Start::Pending {
            starts: 1,
            deadline: now + chrono::TimeDelta::seconds(60),
            new: build("new"),
        };
        let mut watch = Watch::new(paths.clone(), &start, now);
        let (_events_tx, events) = mpsc::channel(1);
        let core = Core::new(&[], None);
        let exit = run(
            &cfg,
            "model",
            core,
            Snapshot::default,
            events,
            Duration::from_secs(2),
            std::future::pending::<()>(),
            &mut watch,
            Updater::disabled(crate::update::Channel::Stable),
        )
        .await
        .unwrap();
        assert_eq!(exit, Exit::Restart);
        assert_eq!(std::fs::read_to_string(&paths.exe).unwrap(), "old build");
        assert!(!paths.pending().exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_connection_completes_the_suspend_at_once() {
        let mut suspend = SuspendWait::new(Duration::from_secs(2));
        let (done, mut rx) = tokio::sync::oneshot::channel();
        suspend.start(done, Some(1), tokio::time::Instant::now());
        suspend.finish();
        assert_eq!(rx.try_recv(), Ok(()));
    }
}
