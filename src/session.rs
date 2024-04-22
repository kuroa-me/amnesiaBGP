use crate::config;
use crate::connection::BgpConn;
use crate::error;
use crate::fsm::*;
use crate::message::*;
use core::time::Duration;
use foundations::telemetry::{init_with_server, log, tracing, TelemetryContext};
use std::cmp::min;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio::select;
use tokio::sync::mpsc;
use tokio::time::timeout;

//TODO: Get rid of those 32 buffer sizes. Use more appropriate values

fn timer_loop() -> (mpsc::Receiver<Event>, mpsc::Sender<Timer>) {
    let (timer_tx, timer_rx) = mpsc::channel::<Event>(32);
    let (timer_ctr_tx, mut timer_ctr_rx) = mpsc::channel::<Timer>(32);

    let timer_kinds = TimerKind::new_vec();
    let mut timers = HashMap::new();

    for kind in timer_kinds.into_iter() {
        let timer_tx = timer_tx.clone();
        let (tx, mut rx) = mpsc::unbounded_channel::<Operation>();
        timers.insert(kind.clone(), tx);
        tokio::spawn(async move {
            let mut dur = FAR_FUTURE;
            loop {
                match timeout(dur, rx.recv()).await {
                    Err(_) => {
                        timer_tx.send(kind.into()).await.unwrap();
                    }
                    Ok(op) => match op {
                        Some(Operation::Stop) => dur = FAR_FUTURE,
                        Some(Operation::Restart) => (),
                        Some(Operation::Update { duration }) => {
                            dur = duration;
                        }
                        _ => (),
                    },
                }
            }
        });
    }

    tokio::spawn(async move {
        loop {
            match timer_ctr_rx.recv().await {
                Some(Timer::ConnectRetry { op }) => {
                    timers
                        .get(&TimerKind::ConnectRetry)
                        .unwrap()
                        .send(op)
                        .unwrap();
                }
                Some(Timer::Hold { op }) => {
                    timers.get(&TimerKind::Hold).unwrap().send(op).unwrap();
                }
                Some(Timer::Keepalive { op }) => {
                    timers.get(&TimerKind::Keepalive).unwrap().send(op).unwrap();
                }
                Some(Timer::DelayOpen { op }) => {
                    timers.get(&TimerKind::DelayOpen).unwrap().send(op).unwrap();
                }
                Some(Timer::IdleHold { op }) => {
                    timers.get(&TimerKind::IdleHold).unwrap().send(op).unwrap();
                }
                None => break,
            }
        }
    });

    (timer_rx, timer_ctr_tx)
}

pub async fn try_tcp_conn(tcp_tx: mpsc::Sender<Event>, peer: SocketAddr) {
    match TcpStream::connect(peer).await {
        Ok(conn) => {
            tcp_tx.send(Event::TcpCRAcked { conn }).await.unwrap();
        }
        //TODO: Add error handling
        Err(_) => {
            tcp_tx.send(Event::TcpConnectionFails {}).await.unwrap();
        }
    }
}

pub async fn run(mut session: State, mut admin_rx: mpsc::Receiver<Event>, conf: config::Neighbor) {
    let (tcp_tx, mut tcp_rx) = mpsc::channel::<Event>(32);
    let (bgp_tx, mut bgp_rx) = mpsc::channel::<Event>(32);
    let (mut timer_rx, timer_ctr_tx) = timer_loop();
    loop {
        let event = select! {
            event = admin_rx.recv() => event.unwrap(),
            event = tcp_rx.recv() => event.unwrap(),
            event = bgp_rx.recv() => event.unwrap(),
            event = timer_rx.recv() => event.unwrap(),
        };
        match (session, event) {
            // Idle State:
            (State::Idle(s), Event::ManualStart {})
            | (State::Idle(s), Event::AutomaticStart {}) => {
                let mut fsm: Fsm<Connect> = s.into();

                fsm.attr.connect_retry_counter = 0;
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Update {
                            duration: DFLT_BGP_TIMERS_CONNECT,
                        },
                    })
                    .await
                    .unwrap();
                fsm.attr.cur_connect_retry_time = DFLT_BGP_TIMERS_CONNECT;

                let tcp_tx = tcp_tx.clone();
                fsm.state.tcp_conn_handle = Some(tokio::spawn(async move {
                    try_tcp_conn(tcp_tx, fsm.peer).await;
                }));
                session = State::Connect(fsm);
            }

            (State::Idle(s), Event::ManualStartWithPassiveTcpEstablishment {})
            | (State::Idle(s), Event::AutomaticStartWithPassiveTcpEstablishment {}) => {
                let mut fsm: Fsm<Active> = s.into();

                fsm.attr.connect_retry_counter = 0;
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Update {
                            duration: DFLT_BGP_TIMERS_CONNECT,
                        },
                    })
                    .await
                    .unwrap();
                fsm.attr.cur_connect_retry_time = DFLT_BGP_TIMERS_CONNECT;

                session = State::Active(fsm);
            }

            // Connect State:
            (State::Connect(s), Event::ManualStop {}) => {
                // Old connection is dropped in the into() function
                let mut fsm: Fsm<Idle> = s.into();

                fsm.attr.connect_retry_counter = 0;
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                fsm.attr.cur_connect_retry_time = FAR_FUTURE;

                session = State::Idle(fsm);
            }

            (State::Connect(mut s), Event::ConnectRetryTimerExpires {}) => {
                // Drop the old connection
                if let Some(handle) = s.state.tcp_conn_handle.take() {
                    handle.abort();
                }

                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Restart,
                    })
                    .await
                    .unwrap();
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_delay_open_time = FAR_FUTURE;

                let tcp_tx = tcp_tx.clone();
                //TODO: The RFC states **Listen**, should we not initiate the connection?
                s.state.tcp_conn_handle = Some(tokio::spawn(async move {
                    try_tcp_conn(tcp_tx, s.peer).await;
                }));
                session = State::Connect(s);
            }

            (State::Connect(s), Event::DelayOpenTimerExpires {}) => {
                // BGP connection is already created, just send the Open message
                let mut fsm: Fsm<OpenSent> = s.into();
                let bgp_out_tx = fsm.state.bgp_out_tx.as_ref().unwrap();
                fsm.state.local_open = Some(OpenMsg::from_conf(&conf));
                bgp_out_tx
                    .send(OpenMsg::from_conf(&conf).into())
                    .await
                    .unwrap();
                session = State::OpenSent(fsm);
            }

            // Not Implemented
            (State::Connect(s), Event::TcpConnectionValid { conn: _ }) => {
                session = State::Connect(s);
            }

            (State::Connect(s), Event::TcpCRInvalid {}) => {
                session = State::Connect(s);
            }

            (State::Connect(mut s), Event::TcpCRAcked { conn })
            | (State::Connect(mut s), Event::TcpConnectionConfirmed { conn }) => {
                s.attr.connect_retry_counter = 0;
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;

                // Create BGP connection from TCP connection
                let bgp_tx = bgp_tx.clone();
                let (bgp_out_tx, bgp_out_rx) = mpsc::channel::<Message>(32);
                s.state.bgp_conn_handle = Some(tokio::spawn(async move {
                    process(conn, bgp_tx, bgp_out_rx).await;
                }));
                s.state.bgp_out_tx = Some(bgp_out_tx.clone());

                if s.attr.delay_open {
                    timer_ctr_tx
                        .send(Timer::DelayOpen {
                            op: Operation::Update {
                                duration: s.attr.delay_open_time,
                            },
                        })
                        .await
                        .unwrap();

                    session = State::Connect(s);
                    continue;
                } else {
                    bgp_out_tx
                        .send(OpenMsg::from_conf(&conf).into())
                        .await
                        .unwrap();
                    let fsm: Fsm<OpenSent> = s.into();
                    //TODO: Sets the HoldTimer to a large value
                    session = State::OpenSent(fsm);
                }
            }

            (State::Connect(mut s), Event::TcpConnectionFails {}) => {
                match s.attr.cur_delay_open_time {
                    FAR_FUTURE => {
                        timer_ctr_tx
                            .send(Timer::ConnectRetry {
                                op: Operation::Stop,
                            })
                            .await
                            .unwrap();
                        s.attr.cur_connect_retry_time = FAR_FUTURE;

                        // All resources are dropped in the into() function
                        session = State::Idle(s.into());
                    }
                    _ => {
                        timer_ctr_tx
                            .send(Timer::ConnectRetry {
                                op: Operation::Restart,
                            })
                            .await
                            .unwrap();
                        timer_ctr_tx
                            .send(Timer::DelayOpen {
                                op: Operation::Stop,
                            })
                            .await
                            .unwrap();
                        s.attr.cur_delay_open_time = FAR_FUTURE;

                        session = State::Active(s.into());
                    }
                }
            }

            (State::Connect(mut s), Event::BGPOpenWithDelayOpenTimerRunning { msg }) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_delay_open_time = FAR_FUTURE;

                //TODO: Finalize BGP connection, do something with the Open message
                let mut fsm: Fsm<OpenConfirm> = s.into();
                fsm.state.peer_open = Some(msg);
                fsm.state.local_open = Some(OpenMsg::from_conf(&conf));

                let bgp_out_tx = fsm.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(fsm.state.local_open.clone().unwrap().into())
                    .await
                    .unwrap();
                bgp_out_tx.send(KeepaliveMsg::new().into()).await.unwrap();

                // Calculate the negotiated hold time
                fsm.attr.hold_time = match Duration::from_secs(
                    min(
                        fsm.state.local_open.as_ref().unwrap().hold_time,
                        fsm.state.peer_open.as_ref().unwrap().hold_time,
                    )
                    .into(),
                ) {
                    FAR_FUTURE => FAR_FUTURE,
                    hold_time => hold_time,
                };

                fsm.attr.keepalive_time = match fsm.attr.hold_time {
                    FAR_FUTURE => FAR_FUTURE,
                    _ => fsm.attr.hold_time / DFLT_KEEPALIVE_FACTOR,
                };

                if fsm.attr.hold_time != FAR_FUTURE {
                    timer_ctr_tx
                        .send(Timer::Keepalive {
                            op: Operation::Update {
                                duration: fsm.attr.keepalive_time,
                            },
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_keepalive_time = fsm.attr.keepalive_time;
                    timer_ctr_tx
                        .send(Timer::Hold {
                            op: Operation::Update {
                                duration: fsm.attr.hold_time,
                            },
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_hold_time = fsm.attr.hold_time;
                } else {
                    timer_ctr_tx
                        .send(Timer::Keepalive {
                            op: Operation::Restart {},
                        })
                        .await
                        .unwrap();
                    timer_ctr_tx
                        .send(Timer::Hold {
                            op: Operation::Stop,
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_hold_time = FAR_FUTURE;
                }

                session = State::OpenConfirm(fsm);
            }

            (State::Connect(mut s), Event::BGPHeaderErr { err })
            | (State::Connect(mut s), Event::BGPOpenMsgErr { err }) => {
                if s.attr.send_notification_without_open {
                    let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                    bgp_out_tx
                        .send(NotificationMsg::from_error(err).into())
                        .await
                        .unwrap();
                }

                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter += 1;

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            (State::Connect(mut s), Event::NotifyMsgVerErr { err }) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;

                let mut fsm: Fsm<Idle> = s.into();

                if fsm.attr.cur_hold_time != FAR_FUTURE {
                    timer_ctr_tx
                        .send(Timer::DelayOpen {
                            op: Operation::Stop,
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_delay_open_time = FAR_FUTURE;
                } else {
                    fsm.attr.connect_retry_counter += 1;
                    if fsm.attr.damp_peer_oscillations {
                        //TODO: Perform peer oscillation damping
                    }
                }

                session = State::Idle(fsm);
            }

            (State::Connect(mut s), _) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_delay_open_time = FAR_FUTURE;

                s.attr.connect_retry_counter += 1;

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            // Active State:
            (State::Active(mut s), Event::ManualStop {}) => {
                if s.attr.cur_hold_time != FAR_FUTURE {
                    timer_ctr_tx
                        .send(Timer::DelayOpen {
                            op: Operation::Stop,
                        })
                        .await
                        .unwrap();
                    s.attr.cur_delay_open_time = FAR_FUTURE;

                    if s.attr.send_notification_without_open {
                        let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                        bgp_out_tx
                            .send(
                                NotificationMsg::from_enum(
                                    error::ErrorCode::Cease,
                                    error::ErrorSubCode::Undefined,
                                    None,
                                )
                                .into(),
                            )
                            .await
                            .unwrap();
                    }
                }

                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter = 0;

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            (State::Active(s), Event::ConnectRetryTimerExpires {}) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Restart,
                    })
                    .await
                    .unwrap();

                let mut fsm: Fsm<Connect> = s.into();

                let tcp_tx = tcp_tx.clone();
                fsm.state.tcp_conn_handle = Some(tokio::spawn(async move {
                    try_tcp_conn(tcp_tx, fsm.peer).await;
                }));

                session = State::Connect(fsm);
            }

            //[Page 60]
            (State::Active(mut s), Event::DelayOpenTimerExpires {}) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_delay_open_time = FAR_FUTURE;

                let mut fsm: Fsm<OpenSent> = s.into();

                let bgp_out_tx = fsm.state.bgp_out_tx.as_ref().unwrap();
                fsm.state.local_open = Some(OpenMsg::from_conf(&conf));
                bgp_out_tx
                    .send(OpenMsg::from_conf(&conf).into())
                    .await
                    .unwrap();

                //TODO: Sets the HoldTimer to a large value
                //[Page 60] Suggests that "AHoldTimer value of 4 minutes is suggested as a "large value" for the HoldTimer.

                session = State::OpenSent(fsm);
            }

            //[Page 60]
            (State::Active(mut s), Event::TcpConnectionFails {}) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Restart,
                    })
                    .await
                    .unwrap();
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_delay_open_time = FAR_FUTURE;
                s.attr.connect_retry_counter += 1;

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 61]
            (State::Active(mut s), Event::BGPOpenWithDelayOpenTimerRunning { msg }) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_delay_open_time = FAR_FUTURE;

                let mut fsm: Fsm<OpenConfirm> = s.into();
                fsm.state.peer_open = Some(msg);

                fsm.state.local_open = Some(OpenMsg::from_conf(&conf));
                let bgp_out_tx = fsm.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(OpenMsg::from_conf(&conf).into())
                    .await
                    .unwrap();
                bgp_out_tx.send(KeepaliveMsg::new().into()).await.unwrap();

                // Calculate the negotiated hold time
                fsm.attr.hold_time = match Duration::from_secs(
                    min(
                        fsm.state.local_open.as_ref().unwrap().hold_time,
                        fsm.state.peer_open.as_ref().unwrap().hold_time,
                    )
                    .into(),
                ) {
                    FAR_FUTURE => FAR_FUTURE,
                    hold_time => hold_time,
                };

                fsm.attr.keepalive_time = match fsm.attr.hold_time {
                    FAR_FUTURE => FAR_FUTURE,
                    _ => fsm.attr.hold_time / DFLT_KEEPALIVE_FACTOR,
                };

                if fsm.attr.hold_time != FAR_FUTURE {
                    timer_ctr_tx
                        .send(Timer::Keepalive {
                            op: Operation::Update {
                                duration: fsm.attr.keepalive_time,
                            },
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_keepalive_time = fsm.attr.keepalive_time;
                    timer_ctr_tx
                        .send(Timer::Hold {
                            op: Operation::Update {
                                duration: fsm.attr.hold_time,
                            },
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_hold_time = fsm.attr.hold_time;
                } else {
                    timer_ctr_tx
                        .send(Timer::Keepalive {
                            op: Operation::Stop,
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_keepalive_time = FAR_FUTURE;
                    timer_ctr_tx
                        .send(Timer::Hold {
                            op: Operation::Stop,
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_hold_time = FAR_FUTURE;
                }

                session = State::OpenConfirm(fsm);
            }

            //[Page 62]
            (State::Active(mut s), Event::BGPHeaderErr { err })
            | (State::Active(mut s), Event::BGPOpenMsgErr { err }) => {
                if s.attr.send_notification_without_open {
                    let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                    bgp_out_tx
                        .send(NotificationMsg::from_error(err).into())
                        .await
                        .unwrap();
                }

                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter += 1;

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 63]
            (State::Active(mut s), Event::NotifyMsgVerErr { err }) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;

                let mut fsm: Fsm<Idle> = s.into();

                if fsm.attr.cur_hold_time != FAR_FUTURE {
                    timer_ctr_tx
                        .send(Timer::DelayOpen {
                            op: Operation::Stop,
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_delay_open_time = FAR_FUTURE;
                } else {
                    fsm.attr.connect_retry_counter += 1;
                    if fsm.attr.damp_peer_oscillations {
                        //TODO: Perform peer oscillation damping
                    }
                }

                session = State::Idle(fsm);
            }

            //[Page 63]
            (State::Active(mut s), _) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;

                s.attr.connect_retry_counter += 1;

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 63]
            // Ignore all start events (Events 1, 3-7)
            (State::OpenSent(s), Event::ManualStart {})
            | (State::OpenSent(s), Event::AutomaticStart {})
            | (State::OpenSent(s), Event::ManualStartWithPassiveTcpEstablishment {})
            | (State::OpenSent(s), Event::AutomaticStartWithPassiveTcpEstablishment {})
            | (State::OpenSent(s), Event::AutomaticStartWithDampPeerOscillations {})
            | (
                State::OpenSent(s),
                Event::AutomaticStartWithDampPeerOscillationsAndPassiveTcpEstablishment {},
            ) => {
                session = State::OpenSent(s);
            }

            //[Page 63]
            (State::OpenSent(mut s), Event::ManualStop {}) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(
                        NotificationMsg::from_enum(
                            error::ErrorCode::Cease,
                            error::ErrorSubCode::Undefined,
                            None,
                        )
                        .into(),
                    )
                    .await
                    .unwrap();

                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter = 0;

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 63]
            (State::OpenSent(mut s), Event::AutomaticStop {}) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(
                        NotificationMsg::from_enum(
                            error::ErrorCode::Cease,
                            error::ErrorSubCode::Undefined,
                            None,
                        )
                        .into(),
                    )
                    .await
                    .unwrap();

                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter += 1;

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 64]
            (State::OpenSent(mut s), Event::HoldTimerExpires {}) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(
                        NotificationMsg::from_enum(
                            error::ErrorCode::HoldTimerExpired,
                            error::ErrorSubCode::Undefined,
                            None,
                        )
                        .into(),
                    )
                    .await
                    .unwrap();

                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter += 1;

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 64]
            (State::OpenSent(s), Event::TcpConnectionValid { conn: _ })
            | (State::OpenSent(s), Event::TcpCRAcked { conn: _ })
            | (State::OpenSent(s), Event::TcpConnectionConfirmed { conn: _ }) => {
                // In our implementation, it is impossible to reach this state
                // because new TCP connection originates from the peers are
                // handled in other logic.

                log::error!("OpenSent state reached with TCP connection event");

                let fsm: Fsm<OpenSent> = s.into();
                session = State::OpenSent(fsm);
            }

            //[Page 64]
            (State::OpenSent(s), Event::TcpCRInvalid {}) => {
                let fsm = s.into();
                session = State::OpenSent(fsm);
            }

            //[Page 64]
            (State::OpenSent(s), Event::TcpConnectionFails {}) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Restart,
                    })
                    .await
                    .unwrap();

                //TODO: The RFC states **Listen**, should we not initiate the connection?

                let fsm: Fsm<Active> = s.into();
                session = State::Active(fsm);
            }

            (State::OpenSent(mut s), Event::BGPOpen { msg }) => {
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_delay_open_time = FAR_FUTURE;
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;

                let mut fsm: Fsm<OpenConfirm> = s.into();

                fsm.state.peer_open = Some(msg);
                // Calculate the negotiated hold time
                fsm.attr.hold_time = match Duration::from_secs(
                    min(
                        fsm.state.local_open.as_ref().unwrap().hold_time,
                        fsm.state.peer_open.as_ref().unwrap().hold_time,
                    )
                    .into(),
                ) {
                    FAR_FUTURE => FAR_FUTURE,
                    hold_time => hold_time,
                };

                if fsm.attr.hold_time != FAR_FUTURE {
                    timer_ctr_tx
                        .send(Timer::Keepalive {
                            op: Operation::Update {
                                duration: fsm.attr.keepalive_time,
                            },
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_keepalive_time = fsm.attr.keepalive_time;
                    timer_ctr_tx
                        .send(Timer::Hold {
                            op: Operation::Update {
                                duration: fsm.attr.hold_time,
                            },
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_hold_time = fsm.attr.hold_time;
                } else {
                    timer_ctr_tx
                        .send(Timer::Keepalive {
                            op: Operation::Restart {},
                        })
                        .await
                        .unwrap();
                    timer_ctr_tx
                        .send(Timer::Hold {
                            op: Operation::Stop,
                        })
                        .await
                        .unwrap();
                    fsm.attr.cur_hold_time = FAR_FUTURE;
                }

                session = State::OpenConfirm(fsm);
            }

            //TODO: Does every single move to Idle state have the timer reset?
            (s, _) => session = s,
        };
    }
}

// TODO: Redundant code
async fn process(
    socket: TcpStream,
    bgp_tx: mpsc::Sender<Event>,
    mut bgp_out_rx: mpsc::Receiver<Message>,
) {
    let mut conn = BgpConn::new(socket);
    loop {
        select! {
            msg = bgp_out_rx.recv() => {
                match msg {
                    Some(msg) => {
                        conn.write_message(msg).await.unwrap();
                    }
                    None => break,
                }
            },
            msg = conn.read_message() => {
                match msg {
                    Ok(msg) => {
                        println!("Received message: {:?}", msg);
                        match msg {
                            Message::Open(open) => {
                                bgp_tx.send(Event::BGPOpen { msg: open }).await.unwrap();
                            }
                            _ => (),
                        }
                    }
                    Err(e) => {
                        println!("Error: {:?}", e);
                        match e.code {
                            error::ErrorCode::MessageHeaderError => {
                                bgp_tx.send(Event::BGPHeaderErr { err: e }).await.unwrap();
                            }
                            error::ErrorCode::OpenMessageError => {
                                match e.subcode {
                                    error::ErrorSubCode::OpenMsgErrorSubcode(error::OpenMsgErrorSubcode::UnsupportedVersionNumber) => {
                                        bgp_tx.send(Event::NotifyMsgVerErr { err: e }).await.unwrap();
                                    }
                                    _ => {
                                        bgp_tx.send(Event::BGPOpenMsgErr { err: e }).await.unwrap();
                                    }
                                }                            },
                            //TODO: Add more error handling
                            _ => break,
                        }
                    }
                }
            }
        }
    }
}
