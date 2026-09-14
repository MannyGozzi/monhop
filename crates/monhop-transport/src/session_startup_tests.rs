use super::{SessionFailure, SessionIo, SessionProgress, handoff_if_ready, run_destination_actor};
use crate::{
    crypto::{DeviceIdentity, LOCAL_TLS_SERVER_NAME, SecureQuicConfig, VerifiedPeer},
    session_actor::{DestinationActor, WatchedDestination},
    session_clock::SessionClock,
    session_handshake::{
        HandshakeConfig, NegotiatedSession, device_id_from_fingerprint, negotiate,
    },
    session_receiver::{DestinationAction, DestinationFailure, InputDestination},
    session_startup::{ReadyControl, StartupControl},
};
use monhop_core::{DisplayId, NativeInputOwnership, Platform, Point, RevocationSignal};
use monhop_protocol::{
    Capabilities, DeliveryClass, DisplayDescription, DisplayTopology, Frame, Message, SessionEpoch,
};
use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const FACTORY_DELAY: Duration = Duration::from_millis(150);

struct FixtureDestination {
    _ownership: NativeInputOwnership,
    actions: mpsc::SyncSender<DestinationAction>,
}

impl InputDestination for FixtureDestination {
    fn apply(&mut self, action: DestinationAction) -> Result<(), DestinationFailure> {
        let _ = self.actions.try_send(action);
        Ok(())
    }
}

impl WatchedDestination for FixtureDestination {
    fn validate_environment(&mut self) -> Result<(), DestinationFailure> {
        Ok(())
    }
}

fn lock_native_input() -> std::sync::MutexGuard<'static, ()> {
    crate::NATIVE_OWNERSHIP_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn topology(display_id: u64) -> DisplayTopology {
    DisplayTopology::new(vec![DisplayDescription {
        id: DisplayId(display_id),
        name: "loopback fixture".to_owned(),
        native_width: 1_920,
        native_height: 1_080,
        logical_origin: Point::default(),
        logical_size: Point::new(1_920.0, 1_080.0),
        scale_factor: 1.0,
        is_primary: true,
        monitor: None,
    }])
    .expect("fixture topology is valid")
}

fn capabilities() -> Capabilities {
    Capabilities::new(Capabilities::RELATIVE_MOTION | Capabilities::DISPLAY_TOPOLOGY)
        .expect("fixture capabilities are known")
}

fn pin(identity: &DeviceIdentity) -> VerifiedPeer {
    VerifiedPeer::from_certificate_der(
        identity.certificate_der(),
        &identity.fingerprint().full_hex(),
    )
    .expect("generated identity has a full matching pin")
}

async fn negotiated_sessions(
    source_is_dialer: bool,
) -> (
    quinn::Endpoint,
    quinn::Endpoint,
    NegotiatedSession,
    NegotiatedSession,
) {
    let client_identity = DeviceIdentity::generate().expect("client identity");
    let server_identity = DeviceIdentity::generate().expect("server identity");
    let server_pin = pin(&server_identity);
    let client_pin = pin(&client_identity);
    let loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let server = quinn::Endpoint::server(
        SecureQuicConfig::server(&server_identity, &client_pin).expect("server TLS configuration"),
        loopback,
    )
    .expect("loopback server endpoint");
    let mut client = quinn::Endpoint::client(loopback).expect("loopback client endpoint");
    client.set_default_client_config(
        SecureQuicConfig::client(&client_identity, &server_pin).expect("client TLS configuration"),
    );
    let connecting = client
        .connect(
            server.local_addr().expect("server address"),
            LOCAL_TLS_SERVER_NAME,
        )
        .expect("loopback connection");
    let (client_connection, server_connection) = tokio::join!(
        async { connecting.await.expect("client TLS connection") },
        async {
            server
                .accept()
                .await
                .expect("incoming connection")
                .await
                .expect("server TLS connection")
        },
    );
    let source = device_id_from_fingerprint(if source_is_dialer {
        client_identity.fingerprint()
    } else {
        server_identity.fingerprint()
    });
    let client_topology = topology(1);
    let server_topology = topology(2);
    let client_config = HandshakeConfig::new(
        &client_identity,
        &server_pin,
        Platform::Windows,
        Platform::MacOs,
        capabilities(),
        capabilities(),
        &client_topology,
        source,
        monhop_protocol::SessionPurpose::Share,
    )
    .expect("client handshake configuration");
    let server_config = HandshakeConfig::new(
        &server_identity,
        &client_pin,
        Platform::MacOs,
        Platform::Windows,
        capabilities(),
        capabilities(),
        &server_topology,
        source,
        monhop_protocol::SessionPurpose::Share,
    )
    .expect("server handshake configuration");
    let (client_session, server_session) = tokio::join!(
        negotiate(client_connection, client_config),
        negotiate(server_connection, server_config),
    );
    (
        client,
        server,
        client_session.expect("client session handshake"),
        server_session.expect("server session handshake"),
    )
}

async fn source_ready_then_activate(
    mut io: SessionIo,
    epoch: SessionEpoch,
    next_sequence: u64,
    mut input_epoch: crate::session_handshake::InputEpochContinuation,
    destination_display: DisplayId,
) -> Result<(SessionIo, usize), SessionFailure> {
    let origin = SessionClock::try_now().unwrap();
    let mut startup = Some(StartupControl::new(
        epoch,
        next_sequence,
        true,
        Duration::ZERO,
    ));
    let mut ready_control = None;
    let mut heartbeat_responses = 0;
    let mut tick = tokio::time::interval(super::SESSION_POLL_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = tick.tick() => {
                io.check()?;
                if let Some(startup) = startup.as_mut()
                    && let Some(frame) = startup.poll(origin.elapsed()).map_err(|_| SessionFailure::Source)?
                {
                    io.send(frame)?;
                }
            }
            frame = io.next_frame() => {
                let frame = frame?;
                let now = origin.elapsed();
                if startup.is_some() {
                    let should_start = {
                        let startup_control = startup.as_mut().expect("startup remains until ready");
                        if matches!(frame.message, Message::Ping(_)) {
                            heartbeat_responses += 1;
                        }
                        if let Some(response) = startup_control
                            .receive(&frame, now)
                            .map_err(|_| SessionFailure::Source)?
                        {
                            io.send(response)?;
                        }
                        startup_control
                            .can_prepare_source(now)
                            .map_err(|_| SessionFailure::Source)?
                    };
                    if should_start {
                        let mut startup = startup.take().expect("startup remains until ready");
                        io.send(startup.announce_ready(now).map_err(|_| SessionFailure::Source)?)?;
                        ready_control = Some(startup.into_ready(now).map_err(|_| SessionFailure::Source)?);
                        let activation_epoch = input_epoch
                            .begin_activation()
                            .map_err(|_| SessionFailure::Source)?;
                        io.send(Frame::new(
                            activation_epoch,
                            0,
                            Message::ActivateDisplayAt {
                                display_id: destination_display,
                                position: Point::new(24.0, 24.0),
                            },
                        ))?;
                    }
                } else if matches!(frame.message, Message::ActivationAck(display) if display == destination_display) {
                    return Ok((io, heartbeat_responses));
                } else if let Some(control) = ready_control.as_mut()
                    && let Some(response) = receive_post_startup_control(control, &frame, now)?
                {
                    io.send(response)?;
                }
            }
        }
    }
}

fn receive_post_startup_control(
    control: &mut ReadyControl,
    frame: &Frame,
    now: Duration,
) -> Result<Option<Frame>, SessionFailure> {
    control
        .health
        .check(now)
        .map_err(|_| SessionFailure::Source)?;
    control
        .limiter
        .allow_at(crate::session_clock::millis_u64(now))
        .map_err(|_| SessionFailure::Source)?;
    if frame.epoch != control.epoch
        || frame.delivery() != DeliveryClass::Reliable
        || control
            .last_heartbeat_in
            .is_some_and(|last| frame.sequence <= last)
    {
        return Err(SessionFailure::Source);
    }
    control.last_heartbeat_in = Some(frame.sequence);
    match frame.message {
        Message::Ping(token) => {
            let sequence = control.next_heartbeat_out;
            control.next_heartbeat_out = sequence.checked_add(1).ok_or(SessionFailure::Source)?;
            Ok(Some(Frame::new(
                control.epoch,
                sequence,
                Message::Pong(token),
            )))
        }
        Message::Pong(token) => {
            control
                .health
                .receive_pong(token, now)
                .map_err(|_| SessionFailure::Source)?;
            Ok(None)
        }
        _ => Err(SessionFailure::Source),
    }
}

fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while !condition() {
        assert!(Instant::now() < deadline, "fixture timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn delayed_native_receiver_keeps_authenticated_heartbeats_and_acks_immediate_activation() {
    let _test = lock_native_input();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
    tokio::time::timeout(Duration::from_secs(5), async {
        for source_is_dialer in [true, false] {
            let (client, server, client_session, server_session) =
                negotiated_sessions(source_is_dialer).await;
            let (source_session, destination_session) = if source_is_dialer {
                (client_session, server_session)
            } else {
                (server_session, client_session)
            };
            let epoch = source_session.initial_epoch;
            let source_sequence = source_session.control.next_sequence();
            let source_input_epoch = source_session.input_epochs;
            let destination_sequence = destination_session.control.next_sequence();
            let destination_displays = destination_session.local.topology.clone();
            let destination_display = destination_displays.displays()[0].id;
            let source_io = SessionIo::new(source_session, SessionProgress::default());
            let destination_io = SessionIo::new(destination_session, SessionProgress::default());
            let (actions, received_actions) = mpsc::sync_channel(8);
            let ownership = NativeInputOwnership::claim().expect("test owns native input");
            let destination = DestinationActor::start_after_local_enable(
                destination_displays,
                RevocationSignal::default(),
                ownership,
                move |ownership| {
                    thread::sleep(FACTORY_DELAY);
                    Ok(FixtureDestination {
                        _ownership: ownership,
                        actions,
                    })
                },
            )
            .expect("actor starts without waiting for native factory");
            let revocation = RevocationSignal::default();
            let destination_runtime = run_destination_actor(
                destination_io,
                destination,
                revocation.clone(),
                SessionProgress::default(),
                epoch,
                destination_sequence,
                None,
            );
            let source_runtime = source_ready_then_activate(
                source_io,
                epoch,
                source_sequence,
                source_input_epoch,
                destination_display,
            );
            tokio::pin!(destination_runtime);
            tokio::pin!(source_runtime);
            let (source_io, heartbeat_responses) = tokio::select! {
                result = &mut source_runtime => result.expect("source receives activation acknowledgement"),
                result = &mut destination_runtime => panic!("destination ended before source activation: {result:?}"),
            };
            assert!(
                heartbeat_responses >= 3,
                "the source must keep answering control heartbeats while construction is delayed"
            );
            assert!(matches!(
                received_actions
                    .recv_timeout(Duration::from_millis(100))
                    .expect("activation reaches fake destination"),
                DestinationAction::MoveTo(_)
            ));
            revocation.revoke();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), &mut destination_runtime)
                    .await
                    .expect("destination shutdown completes"),
                Err(SessionFailure::Revoked)
            );
            drop(source_io);
            client.close(0_u32.into(), b"test complete");
            server.close(0_u32.into(), b"test complete");
            assert!(!NativeInputOwnership::is_claimed());
        }
    })
    .await
    .expect("loopback startup fixture completes");
    });
}

#[test]
fn source_ready_handoffs_before_an_immediate_activation_is_submitted() {
    let _test = lock_native_input();
    let (actions, received_actions) = mpsc::sync_channel(8);
    let ownership = NativeInputOwnership::claim().expect("test owns native input");
    let displays = topology(1);
    let mut actor = DestinationActor::start_after_local_enable(
        displays,
        RevocationSignal::default(),
        ownership,
        move |ownership| {
            Ok(FixtureDestination {
                _ownership: ownership,
                actions,
            })
        },
    )
    .expect("actor starts");
    wait_until(|| actor.is_native_ready());
    let epoch = SessionEpoch::new(11).expect("nonzero epoch");
    let mut source = StartupControl::new(epoch, 3, true, Duration::ZERO);
    let mut receiver = StartupControl::new(epoch, 3, false, Duration::ZERO);
    let source_ping = source.poll(Duration::ZERO).unwrap().unwrap();
    let receiver_pong = receiver
        .receive(&source_ping, Duration::ZERO)
        .unwrap()
        .unwrap();
    source.receive(&receiver_pong, Duration::ZERO).unwrap();
    let receiver_ping = receiver.poll(Duration::ZERO).unwrap().unwrap();
    let source_pong = source
        .receive(&receiver_ping, Duration::ZERO)
        .unwrap()
        .unwrap();
    receiver.receive(&source_pong, Duration::ZERO).unwrap();
    let receiver_ready = receiver.announce_ready(Duration::ZERO).unwrap();
    source.receive(&receiver_ready, Duration::ZERO).unwrap();
    let source_ready = source.announce_ready(Duration::ZERO).unwrap();
    receiver.receive(&source_ready, Duration::ZERO).unwrap();
    let origin = SessionClock::try_now().unwrap();
    let mut receiver = Some(receiver);
    assert!(handoff_if_ready(&mut receiver, &actor, &origin, None).unwrap());
    assert!(receiver.is_none());
    actor
        .try_submit(Frame::new(
            SessionEpoch::new(12).unwrap(),
            0,
            Message::ActivateDisplayAt {
                display_id: DisplayId(1),
                position: Point::new(24.0, 24.0),
            },
        ))
        .expect("activation is admitted after the synchronous handoff");
    wait_until(|| actor.is_started());
    let response_deadline = Instant::now() + Duration::from_millis(500);
    let response = loop {
        assert!(
            Instant::now() < response_deadline,
            "actor response deadline"
        );
        if let Some(frame) = actor.try_response().unwrap() {
            break frame;
        }
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        response,
        Frame::new(
            SessionEpoch::new(12).unwrap(),
            0,
            Message::ActivationAck(DisplayId(1))
        )
    );
    assert!(matches!(
        received_actions
            .recv_timeout(Duration::from_millis(100))
            .expect("activation reaches fake destination"),
        DestinationAction::MoveTo(_)
    ));
    actor.request_stop();
    wait_until(|| actor.finish());
    assert!(!NativeInputOwnership::is_claimed());
}

#[test]
fn cancellation_during_delayed_factory_retains_ownership_until_cleanup_exits() {
    let _test = lock_native_input();
    let (factory_started, wait_for_factory) = mpsc::sync_channel(1);
    let (release_factory, wait_for_release) = mpsc::sync_channel(1);
    let (actions, _) = mpsc::sync_channel(8);
    let revocation = RevocationSignal::default();
    let ownership = NativeInputOwnership::claim().expect("test owns native input");
    let mut actor = DestinationActor::start_after_local_enable(
        topology(1),
        revocation.clone(),
        ownership,
        move |ownership| {
            factory_started
                .send(())
                .expect("factory start notification");
            wait_for_release
                .recv_timeout(Duration::from_secs(1))
                .expect("factory release");
            Ok(FixtureDestination {
                _ownership: ownership,
                actions,
            })
        },
    )
    .expect("actor starts immediately");
    wait_for_factory
        .recv_timeout(Duration::from_millis(100))
        .expect("factory started on actor thread");
    revocation.revoke();
    assert!(NativeInputOwnership::is_claimed());
    release_factory.send(()).expect("release factory");
    wait_until(|| actor.finish());
    assert!(!NativeInputOwnership::is_claimed());
}

#[test]
fn input_before_ready_handoff_is_rejected_without_native_delivery() {
    let _test = lock_native_input();
    let (actions, received_actions) = mpsc::sync_channel(8);
    let ownership = NativeInputOwnership::claim().expect("test owns native input");
    let mut actor = DestinationActor::start_after_local_enable(
        topology(1),
        RevocationSignal::default(),
        ownership,
        move |ownership| {
            Ok(FixtureDestination {
                _ownership: ownership,
                actions,
            })
        },
    )
    .expect("actor starts");
    wait_until(|| actor.is_native_ready());
    assert!(
        actor
            .try_submit(Frame::new(
                SessionEpoch::new(2).unwrap(),
                0,
                Message::ActivateDisplayAt {
                    display_id: DisplayId(1),
                    position: Point::new(24.0, 24.0),
                },
            ))
            .is_err()
    );
    wait_until(|| actor.finish());
    assert!(
        received_actions
            .try_iter()
            .all(|action| matches!(action, DestinationAction::ReleaseAll))
    );
    assert!(!NativeInputOwnership::is_claimed());
}
