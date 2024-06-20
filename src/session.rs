use crate::config;
use crate::connection::BgpConn;
use crate::error;
use crate::fsm::*;
use crate::message::*;
use core::panic;
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

fn timer_loop() -> (mpsc::Sender<Timer>, mpsc::Receiver<Event>) {
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

    (timer_ctr_tx, timer_rx)
}

pub async fn try_tcp_conn(tcp_tx: mpsc::Sender<Event>, peer: SocketAddr) {
    match TcpStream::connect(peer).await {
        Ok(conn) => {
            tcp_tx.send(Event::TcpCRAcked { conn }).await.unwrap();
        }
        //TODO: Add error handling
        Err(e) => {
            tcp_tx
                .send(Event::TcpConnectionFails { err: e.into() })
                .await
                .unwrap();
        }
    }
}

pub enum Source {
    Admin,
    Tcp,
    Bgp,
    Coll,
    Timer,
}

pub async fn run(mut session: State, mut admin_rx: mpsc::Receiver<Event>, conf: config::Neighbor) {
    let (tcp_tx, mut tcp_rx) = mpsc::channel::<Event>(32);
    let (bgp_tx, mut bgp_rx) = mpsc::channel::<Event>(32);
    let (coll_tx, mut coll_rx) = mpsc::channel::<Event>(32);
    let (timer_ctr_tx, mut timer_rx) = timer_loop();
    loop {
        let mut source = Source::Admin;
        let event = select! {
            event = admin_rx.recv() => {
                source = Source::Admin;
                match event.unwrap() {
                // Admin channel is also utilized for passing passively established
                // BGP connections. But before passing in, the FSM attributes are
                // unknown. Thus, we need to modify the event to fit the FSM.
                Event::BGPOpen { msg, remote_syn } if session.get_attr().cur_delay_open_time != FAR_FUTURE => {
                    Event::BGPOpenWithDelayOpenTimerRunning { msg, remote_syn }
                },
                e @ _ => e,
            }},
            event = tcp_rx.recv() => {
                source = Source::Tcp;
                event.unwrap()
            },
            event = bgp_rx.recv() => {
                source = Source::Bgp;
                event.unwrap()
            },
            event = coll_rx.recv() => {
                source = Source::Coll;
                event.unwrap()
            },
            event = timer_rx.recv() => {
                source = Source::Timer;
                event.unwrap()
            },
        };
        match (session, event, source) {
            // Idle State:
            //[Page 53]
            (State::Idle(s), Event::ManualStart {}, _)
            | (State::Idle(s), Event::AutomaticStart {}, _) => {
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

            //[Page 54]
            (State::Idle(s), Event::ManualStartWithPassiveTcpEstablishment {}, _)
            | (State::Idle(s), Event::AutomaticStartWithPassiveTcpEstablishment {}, _) => {
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
            //[Page 54]
            (State::Connect(s), Event::ManualStop {}, _) => {
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

            //[Page 55]
            (State::Connect(mut s), Event::ConnectRetryTimerExpires {}, _) => {
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

            //[Page 55]
            (State::Connect(s), Event::DelayOpenTimerExpires {}, _) => {
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

            //[Page 55]
            // Not Implemented
            (State::Connect(s), Event::TcpConnectionValid {}, _) => {
                session = State::Connect(s);
            }

            //[Page 55]
            (State::Connect(s), Event::TcpCRInvalid {}, _) => {
                session = State::Connect(s);
            }

            //[Page 55]
            (State::Connect(mut s), Event::TcpCRAcked { conn }, _) => {
                let remote_syn = false;

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
                    let bgp_conn = BgpConn::new(conn);
                    process(bgp_conn, bgp_tx, bgp_out_rx, remote_syn).await;
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
                    s.attr.cur_delay_open_time = s.attr.delay_open_time;

                    session = State::Connect(s);
                    continue;
                } else {
                    bgp_out_tx
                        .send(OpenMsg::from_conf(&conf).into())
                        .await
                        .unwrap();
                    let mut fsm: Fsm<OpenSent> = s.into();
                    fsm.state.remote_syn = remote_syn;
                    //TODO: Sets the HoldTimer to a large value
                    session = State::OpenSent(fsm);
                }
            }

            //[Page 55]
            (State::Connect(mut s), Event::TcpConnectionConfirmed { bgp_conn }, _) => {
                let remote_syn = true;

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
                    process(bgp_conn, bgp_tx, bgp_out_rx, remote_syn).await;
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
                    let mut fsm: Fsm<OpenSent> = s.into();
                    fsm.state.remote_syn = remote_syn;
                    //TODO: Sets the HoldTimer to a large value
                    session = State::OpenSent(fsm);
                }
            }

            //[Page 56]
            (State::Connect(mut s), Event::TcpConnectionFails { err: _ }, _) => {
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

            //[Page 56]
            (
                State::Connect(mut s),
                Event::BGPOpenWithDelayOpenTimerRunning { msg, remote_syn },
                _,
            ) => {
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

            //[Page 57]
            (State::Connect(mut s), Event::BGPHeaderErr { err }, _)
            | (State::Connect(mut s), Event::BGPOpenMsgErr { err }, _) => {
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

            //[Page 57]
            (State::Connect(mut s), Event::NotifyMsgVerErr { err }, _) => {
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

            //[Page 58]
            (State::Connect(mut s), _, _) => {
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
            //[Page 59]
            // Ignore all start events (Events 1, 3-7)
            (State::Active(s), Event::ManualStart {}, _)
            | (State::Active(s), Event::AutomaticStart {}, _)
            | (State::Active(s), Event::ManualStartWithPassiveTcpEstablishment {}, _)
            | (State::Active(s), Event::AutomaticStartWithPassiveTcpEstablishment {}, _)
            | (State::Active(s), Event::AutomaticStartWithDampPeerOscillations {}, _)
            | (
                State::Active(s),
                Event::AutomaticStartWithDampPeerOscillationsAndPassiveTcpEstablishment {},
                _,
            ) => {
                session = State::Active(s);
            }

            //[Page 59]
            (State::Active(mut s), Event::ManualStop {}, _) => {
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

            //[Page 59]
            (State::Active(s), Event::ConnectRetryTimerExpires {}, _) => {
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

            //[Page 59]
            (State::Active(mut s), Event::DelayOpenTimerExpires {}, _) => {
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
            (State::Active(s), Event::TcpConnectionValid {}, _) => {
                session = State::Active(s);
            }

            //[Page 60]
            (State::Active(s), Event::TcpCRInvalid {}, _) => {
                session = State::Active(s);
            }

            //[Page 60]
            (State::Active(mut s), Event::TcpCRAcked { conn }, _) => {
                let remote_syn = false;

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
                    let bgp_conn = BgpConn::new(conn);
                    process(bgp_conn, bgp_tx, bgp_out_rx, remote_syn).await;
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
                    s.attr.cur_delay_open_time = s.attr.delay_open_time;

                    session = State::Active(s);
                    continue;
                } else {
                    bgp_out_tx
                        .send(OpenMsg::from_conf(&conf).into())
                        .await
                        .unwrap();
                    let mut fsm: Fsm<OpenSent> = s.into();
                    fsm.state.remote_syn = remote_syn;
                    //TODO: Sets the HoldTimer to a large value
                    session = State::OpenSent(fsm);
                }
            }

            //[Page 60]
            (State::Active(mut s), Event::TcpConnectionConfirmed { bgp_conn }, _) => {
                let remote_syn = true;

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
                    process(bgp_conn, bgp_tx, bgp_out_rx, remote_syn).await;
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
                    s.attr.cur_delay_open_time = s.attr.delay_open_time;

                    session = State::Active(s);
                    continue;
                } else {
                    bgp_out_tx
                        .send(OpenMsg::from_conf(&conf).into())
                        .await
                        .unwrap();
                    let mut fsm: Fsm<OpenSent> = s.into();
                    fsm.state.remote_syn = remote_syn;
                    //TODO: Sets the HoldTimer to a large value
                    session = State::OpenSent(fsm);
                }
            }

            //[Page 60]
            (State::Active(mut s), Event::TcpConnectionFails { err: _ }, _) => {
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
            (
                State::Active(mut s),
                Event::BGPOpenWithDelayOpenTimerRunning { msg, remote_syn },
                _,
            ) => {
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
            (State::Active(mut s), Event::BGPHeaderErr { err }, _)
            | (State::Active(mut s), Event::BGPOpenMsgErr { err }, _) => {
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
            (State::Active(mut s), Event::NotifyMsgVerErr { err }, _) => {
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
            (State::Active(mut s), _, _) => {
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

            // OpenSent State:
            //[Page 63]
            // Ignore all start events (Events 1, 3-7)
            (State::OpenSent(s), Event::ManualStart {}, _)
            | (State::OpenSent(s), Event::AutomaticStart {}, _)
            | (State::OpenSent(s), Event::ManualStartWithPassiveTcpEstablishment {}, _)
            | (State::OpenSent(s), Event::AutomaticStartWithPassiveTcpEstablishment {}, _)
            | (State::OpenSent(s), Event::AutomaticStartWithDampPeerOscillations {}, _)
            | (
                State::OpenSent(s),
                Event::AutomaticStartWithDampPeerOscillationsAndPassiveTcpEstablishment {},
                _,
            ) => {
                session = State::OpenSent(s);
            }

            //[Page 63]
            (State::OpenSent(mut s), Event::ManualStop {}, _) => {
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
            (State::OpenSent(mut s), Event::AutomaticStop {}, _) => {
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
            (State::OpenSent(mut s), Event::HoldTimerExpires {}, _) => {
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
            (State::OpenSent(s), Event::TcpConnectionValid {}, _) => {
                session = State::OpenSent(s);
            }

            //[Page 64]
            (State::OpenSent(s), Event::TcpCRAcked { conn }, _) => {
                let coll_tx = coll_tx.clone();
                tokio::spawn(async move {
                    let bgp_conn = BgpConn::new(conn);
                    process_one(bgp_conn, coll_tx, false).await;
                    //The bgp_conn will be returned in a custom event
                });
                session = State::OpenSent(s);
            }

            //[Page 64]
            (State::OpenSent(mut s), Event::TcpConnectionConfirmed { bgp_conn }, _) => {
                s.state.collision_conn = Some(bgp_conn);
                session = State::OpenSent(s);
            }

            // Custom Event:
            (State::OpenSent(mut s), Event::ProcessOneDone { conn }, _) => {
                s.state.collision_conn = Some(conn);
                session = State::OpenSent(s);
            }

            //[Page 64]
            (State::OpenSent(s), Event::TcpCRInvalid {}, _) => {
                session = State::OpenSent(s);
            }

            //[Page 64]
            (State::OpenSent(s), Event::TcpConnectionFails { err: _ }, _) => {
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

            //[Page 65]
            (State::OpenSent(mut s), Event::BGPOpen { msg, remote_syn }, _) => {
                // Performing the collision detection
                if s.state.remote_syn != remote_syn {
                    if s.state.local_open.as_ref().unwrap().bgp_identifier < msg.bgp_identifier {
                        // Replace current connection with the new colliding connection
                        let bgp_tx = bgp_tx.clone();
                        let (bgp_out_tx, bgp_out_rx) = mpsc::channel::<Message>(32);
                        let bgp_conn = s.state.collision_conn.take().unwrap();
                        let old_out_tx = s.state.bgp_out_tx.replace(bgp_out_tx.clone()).unwrap();
                        old_out_tx
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

                        //TODO: It might be possible that the handle is dropped before sending out the Cease message.
                        let _ = s.state.bgp_conn_handle.replace(tokio::spawn(async move {
                            process(bgp_conn, bgp_tx, bgp_out_rx, remote_syn).await;
                        }));

                        // Sync
                        if remote_syn {
                            let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                            bgp_out_tx
                                .send(OpenMsg::from_conf(&conf).into())
                                .await
                                .unwrap();
                        }
                    } else {
                        // Drop the colliding connection
                        _ = s.state.collision_conn.take();
                    }
                }

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

            //[Page 65]
            (State::OpenSent(mut s), Event::BGPHeaderErr { err }, Source::Bgp)
            | (State::OpenSent(mut s), Event::BGPOpenMsgErr { err }, Source::Bgp) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(NotificationMsg::from_error(err).into())
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

            //[Page 65]
            // Ignore colliding connection's Error messages
            (State::OpenSent(s), Event::BGPHeaderErr { err }, src)
            | (State::OpenSent(s), Event::BGPOpenMsgErr { err }, src) => {
                session = State::OpenSent(s);
            }

            //[Page 66]
            (State::OpenSent(mut s), Event::NotifyMsgVerErr { err }, _) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 66]
            (State::OpenSent(mut s), _, _) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(
                        NotificationMsg::from_enum(
                            error::ErrorCode::FiniteStateMachineError,
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

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            // OpenConfirm State:
            //[Page 67]
            // Ignore all start events (Events 1, 3-7)
            (State::OpenConfirm(s), Event::ManualStart {}, _)
            | (State::OpenConfirm(s), Event::AutomaticStart {}, _)
            | (State::OpenConfirm(s), Event::ManualStartWithPassiveTcpEstablishment {}, _)
            | (State::OpenConfirm(s), Event::AutomaticStartWithPassiveTcpEstablishment {}, _)
            | (State::OpenConfirm(s), Event::AutomaticStartWithDampPeerOscillations {}, _)
            | (
                State::OpenConfirm(s),
                Event::AutomaticStartWithDampPeerOscillationsAndPassiveTcpEstablishment {},
                _,
            ) => {
                session = State::OpenConfirm(s);
            }

            //[Page 67]
            (State::OpenConfirm(mut s), Event::ManualStop {}, _) => {
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

            //[Page 67]
            (State::OpenConfirm(mut s), Event::AutomaticStop {}, _) => {
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

            //[Page 67]
            (State::OpenConfirm(mut s), Event::HoldTimerExpires {}, _) => {
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

            //[Page 68]
            (State::OpenConfirm(s), Event::KeepaliveTimerExpires {}, _) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx.send(KeepaliveMsg::new().into()).await.unwrap();

                timer_ctr_tx
                    .send(Timer::Keepalive {
                        op: Operation::Restart {},
                    })
                    .await
                    .unwrap();

                session = State::OpenConfirm(s);
            }

            //[Page 68]
            (State::OpenConfirm(s), Event::TcpConnectionValid {}, _) => {
                session = State::OpenConfirm(s);
            }

            //[Page 68]
            (State::OpenConfirm(s), Event::TcpCRAcked { conn }, _) => {
                let coll_tx = coll_tx.clone();
                tokio::spawn(async move {
                    let bgp_conn = BgpConn::new(conn);
                    process_one(bgp_conn, coll_tx, false).await;
                });

                session = State::OpenConfirm(s);
            }

            //[Page 68]
            (State::OpenConfirm(mut s), Event::TcpConnectionConfirmed { bgp_conn: conn }, _) => {
                s.state.collision_conn = Some(conn);
                session = State::OpenConfirm(s);
            }

            // Custom Event:
            (State::OpenConfirm(mut s), Event::ProcessOneDone { conn }, _) => {
                s.state.collision_conn = Some(conn);
                session = State::OpenConfirm(s);
            }

            //[Page 68]
            (State::OpenConfirm(s), Event::TcpCRInvalid {}, _) => {
                // The local system will ignore the second connection attempt.

                let fsm = s.into();
                session = State::OpenConfirm(fsm);
            }

            //[Page 68]
            (State::OpenConfirm(mut s), Event::TcpConnectionFails { err: _ }, _) => {
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

            //[Page 69]
            (State::OpenConfirm(mut s), Event::NotifyMsgVerErr { err }, _) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 69]
            (State::OpenConfirm(mut s), Event::BGPOpen { msg, remote_syn }, _) => {
                // Performing the collision detection
                if s.state.remote_syn == remote_syn {
                    log::error!("Received OPEN message from the same direction");
                    //TODO: Fix this with other unwraps
                    panic!("Multiple OPEN messages received");
                }

                if s.state.local_open.as_ref().unwrap().bgp_identifier < msg.bgp_identifier {
                    // Replace current connection with the new colliding connection
                    let bgp_tx = bgp_tx.clone();
                    let (bgp_out_tx, bgp_out_rx) = mpsc::channel::<Message>(32);
                    let bgp_conn = s.state.collision_conn.take().unwrap();
                    s.state.bgp_conn_handle.replace(tokio::spawn(async move {
                        process(bgp_conn, bgp_tx, bgp_out_rx, remote_syn).await;
                    }));
                    s.state.bgp_out_tx.replace(bgp_out_tx.clone());
                } else {
                    // Drop the colliding connection
                    _ = s.state.collision_conn.take();
                }

                session = State::OpenConfirm(s);
            }

            //[Page 69]
            (State::OpenConfirm(mut s), Event::BGPHeaderErr { err }, Source::Bgp)
            | (State::OpenConfirm(mut s), Event::BGPOpenMsgErr { err }, Source::Bgp) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(NotificationMsg::from_error(err).into())
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

            //[Page 69]
            // Ignore colliding connection's Error messages
            (State::OpenConfirm(s), Event::BGPHeaderErr { err }, src)
            | (State::OpenConfirm(s), Event::BGPOpenMsgErr { err }, src) => {
                session = State::OpenConfirm(s);
            }

            //[Page 70]
            (State::OpenConfirm(s), Event::OpenCollisionDump {}, _) => {
                // Not implemented
                session = State::OpenConfirm(s);
            }

            //[Page 70]
            (State::OpenConfirm(mut s), Event::KeepaliveMsg {}, _) => {
                timer_ctr_tx
                    .send(Timer::Hold {
                        op: Operation::Restart,
                    })
                    .await
                    .unwrap();

                let fsm: Fsm<Established> = s.into();
                session = State::Established(fsm);
            }

            //[Page 70]
            (State::OpenConfirm(mut s), _, _) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(
                        NotificationMsg::from_enum(
                            error::ErrorCode::FiniteStateMachineError,
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

            // Established State:
            //[Page 71]
            // Ignore all start events (Events 1, 3-7)
            (State::Established(s), Event::ManualStart {}, _)
            | (State::Established(s), Event::AutomaticStart {}, _)
            | (State::Established(s), Event::ManualStartWithPassiveTcpEstablishment {}, _)
            | (State::Established(s), Event::AutomaticStartWithPassiveTcpEstablishment {}, _)
            | (State::Established(s), Event::AutomaticStartWithDampPeerOscillations {}, _)
            | (
                State::Established(s),
                Event::AutomaticStartWithDampPeerOscillationsAndPassiveTcpEstablishment {},
                _,
            ) => {
                session = State::Established(s);
            }

            //[Page 71]
            (State::Established(mut s), Event::ManualStop {}, _) => {
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

                //TODO: Delete all routes associated with this connection,

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 71]
            (State::Established(mut s), Event::AutomaticStop {}, _) => {
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

                //TODO: Deletes all routes associated with this connection

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 72]
            (State::Established(mut s), Event::HoldTimerExpires {}, _) => {
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

            //[Page 72]
            (State::Established(mut s), Event::KeepaliveTimerExpires {}, _) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx.send(KeepaliveMsg::new().into()).await.unwrap();

                // restarts its KeepaliveTimer, unless the negotiated HoldTime value is zero.
                if s.attr.hold_time != FAR_FUTURE {
                    timer_ctr_tx
                        .send(Timer::Keepalive {
                            op: Operation::Restart,
                        })
                        .await
                        .unwrap();
                }

                session = State::Established(s);
            }

            //[Page 72]
            (State::Established(s), Event::TcpConnectionValid {}, _) => {
                //Not implemented
                session = State::Established(s);
            }

            //[Page 72]
            (State::Established(s), Event::TcpCRInvalid {}, _) => {
                //Ignored
                session = State::Established(s);
            }

            //[Page 72]
            (State::Established(s), Event::TcpCRAcked { conn }, _) => {
                let coll_tx = coll_tx.clone();
                tokio::spawn(async move {
                    let bgp_conn = BgpConn::new(conn);
                    process_one(bgp_conn, coll_tx, false).await;
                });

                session = State::Established(s);
            }

            //[Page 72]
            (State::Established(mut s), Event::TcpConnectionConfirmed { bgp_conn: conn }, _) => {
                s.state.collision_conn = Some(conn);
                session = State::Established(s);
            }

            // Custom Event:
            (State::Established(mut s), Event::ProcessOneDone { conn }, _) => {
                s.state.collision_conn = Some(conn);
                session = State::Established(s);
            }

            //[Page 73]
            (State::Established(mut s), Event::BGPOpen { msg, remote_syn }, _) => {
                // Performing the collision detection
                if s.state.remote_syn == remote_syn {
                    log::error!("Received OPEN message from the same direction");
                    //TODO: Fix this with other unwraps
                    panic!("Multiple OPEN messages received");
                }

                if s.attr.collision_detect_established_state
                    && s.state.local_open.as_ref().unwrap().bgp_identifier < msg.bgp_identifier
                {
                    // Replace current connection with the new colliding connection
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
                    //TODO: Delete all routes associated with this connection

                    let bgp_tx = bgp_tx.clone();
                    let (bgp_out_tx, bgp_out_rx) = mpsc::channel::<Message>(32);
                    let bgp_conn = s.state.collision_conn.take().unwrap();
                    s.state.bgp_conn_handle.replace(tokio::spawn(async move {
                        process(bgp_conn, bgp_tx, bgp_out_rx, remote_syn).await;
                    }));
                    s.state.bgp_out_tx.replace(bgp_out_tx.clone());
                } else {
                    // Drop the colliding connection
                    _ = s.state.collision_conn.take();
                }

                session = State::Established(s);
            }

            //[Page 73]
            (State::Established(mut s), Event::NotifyMsgVerErr { err }, _)
            | (State::Established(mut s), Event::NotifyMsg { err }, _)
            | (State::Established(mut s), Event::TcpConnectionFails { err }, _) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter += 1;

                //TODO: Delete all routes associated with this connection

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 74]
            (State::Established(mut s), Event::KeepaliveMsg {}, _) => {
                timer_ctr_tx
                    .send(Timer::Hold {
                        op: Operation::Restart,
                    })
                    .await
                    .unwrap();
                session = State::Established(s);
            }

            //[Page 74]
            (State::Established(mut s), Event::UpdateMsg { msg }, _) => {
                //TODO: Process the Update message

                timer_ctr_tx
                    .send(Timer::Hold {
                        op: Operation::Restart,
                    })
                    .await
                    .unwrap();
                session = State::Established(s);
            }

            //[Page 74]
            (State::Established(mut s), Event::UpdateMsgErr { err }, _) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(NotificationMsg::from_error(err).into())
                    .await
                    .unwrap();

                timer_ctr_tx
                    .send(
                        Timer::ConnectRetry {
                            op: Operation::Stop,
                        }
                        .into(),
                    )
                    .await
                    .unwrap();
                s.attr.cur_connect_retry_time = FAR_FUTURE;
                s.attr.connect_retry_counter += 1;

                //TODO: Delete all routes associated with this connection

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //[Page 74]
            (State::Established(mut s), _, _) => {
                let bgp_out_tx = s.state.bgp_out_tx.as_ref().unwrap();
                bgp_out_tx
                    .send(
                        NotificationMsg::from_enum(
                            error::ErrorCode::FiniteStateMachineError,
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

                //TODO: Delete all routes associated with this connection

                if s.attr.damp_peer_oscillations {
                    //TODO: Perform peer oscillation damping
                }

                let fsm: Fsm<Idle> = s.into();
                session = State::Idle(fsm);
            }

            //TODO: Does every single move to Idle state have the timer reset?
            (s, _, _) => session = s,
        };
    }
}

async fn process_one(mut conn: BgpConn, bgp_tx: mpsc::Sender<Event>, remote_syn: bool) {
    let msg = conn.read_message().await;
    match msg {
        Ok(msg) => match msg {
            Message::Open(open) => {
                bgp_tx.send(Event::ProcessOneDone { conn }).await.unwrap();
                bgp_tx
                    .send(Event::BGPOpen {
                        msg: open,
                        remote_syn,
                    })
                    .await
                    .unwrap();
            }
            _ => (),
        },
        Err(e) => {
            match e.code {
                error::ErrorCode::MessageHeaderError => {
                    bgp_tx.send(Event::BGPHeaderErr { err: e }).await.unwrap();
                }
                error::ErrorCode::OpenMessageError => {
                    bgp_tx.send(Event::BGPOpenMsgErr { err: e }).await.unwrap();
                }
                //TODO: Add more error handling
                _ => (),
            }
        }
    }
}

async fn process(
    mut conn: BgpConn,
    bgp_tx: mpsc::Sender<Event>,
    mut bgp_out_rx: mpsc::Receiver<Message>,
    remote_syn: bool,
) {
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
                                bgp_tx.send(Event::BGPOpen { msg: open , remote_syn}).await.unwrap();
                            },
                            Message::Update(update) => {
                                bgp_tx.send(Event::UpdateMsg { msg: update }).await.unwrap();
                            },
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
                                bgp_tx.send(Event::BGPOpenMsgErr { err: e }).await.unwrap();
                            }
                            //TODO: Add more error handling
                            _ => break,
                        }
                    }
                }
            }
        }
    }
}
