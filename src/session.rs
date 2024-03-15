use crate::connection::BgpConn;
use crate::fsm::*;
use core::time::Duration;
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
                        Some(Operation::Reset) => (),
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

pub async fn run(mut session: State, mut admin_rx: mpsc::Receiver<Event>) {
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
                session = State::Active(fsm);
            }

            // Connect State:
            (State::Connect(mut s), Event::ManualStop {}) => {
                s.state.tcp_conn_handle.take();
                let mut fsm: Fsm<Idle> = s.into();
                fsm.attr.connect_retry_counter = 0;
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                session = State::Idle(fsm);
            }

            (State::Connect(mut s), Event::ConnectRetryTimerExpires {}) => {
                // Drop the old connection
                s.state.tcp_conn_handle.take();
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Reset,
                    })
                    .await
                    .unwrap();
                timer_ctr_tx
                    .send(Timer::DelayOpen {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                let tcp_tx = tcp_tx.clone();
                s.state.tcp_conn_handle = Some(tokio::spawn(async move {
                    try_tcp_conn(tcp_tx, s.peer).await;
                }));
                session = State::Connect(s);
            }

            (State::Connect(mut s), Event::DelayOpenTimerExpires {}) => {
                let tcp_conn = s.state.tcp_conn.take().unwrap();
                let mut fsm: Fsm<OpenSent> = s.into();
                let bgp_tx = bgp_tx.clone();
                fsm.state.bgp_conn_handle = Some(tokio::spawn(async move {
                    process(tcp_conn, bgp_tx).await;
                }));
                session = State::OpenSent(fsm);
            }

            // Not Implemented
            (State::Connect(s), Event::TcpConnectionValid {}) => {
                session = State::Connect(s);
            }

            (State::Connect(s), Event::TcpCRInvalid {}) => {
                session = State::Connect(s);
            }

            (State::Connect(mut s), Event::TcpCRAcked { .. }) => {
                timer_ctr_tx
                    .send(Timer::ConnectRetry {
                        op: Operation::Stop,
                    })
                    .await
                    .unwrap();
                s.attr.connect_retry_counter = 0;

                if s.attr.delay_open {
                    timer_ctr_tx
                        .send(Timer::DelayOpen {
                            op: Operation::Update {
                                duration: s.attr.delay_open_time,
                            },
                        })
                        .await
                        .unwrap();
                    // let bgp_tx = bgp_tx.clone();

                    session = State::Connect(s);
                    continue;
                } else {
                    let tcp_conn = s.state.tcp_conn.take().unwrap();
                    let mut fsm: Fsm<OpenSent> = s.into();
                    let bgp_tx = bgp_tx.clone();
                    //TODO: Sends an OPEN message to its peer
                    fsm.state.bgp_conn_handle = Some(tokio::spawn(async move {
                        process(tcp_conn, bgp_tx).await;
                    }));
                    //TODO: Sets the HoldTimer to a large value
                    session = State::OpenSent(fsm);
                }
            }

            (State::Connect(s), Event::TcpConnectionConfirmed { conn }) => {
                let bgp_tx = bgp_tx.clone();
                tokio::spawn(async move {
                    process(conn, bgp_tx).await;
                });
                session = State::Active(s.into());
            }

            (s, _) => session = s,
        };
    }
}

// TODO: Redundant code
async fn process(socket: TcpStream, bgp_tx: mpsc::Sender<Event>) {
    let mut conn = BgpConn::new(socket);
    loop {
        match conn.read_message().await {
            Ok(msg) => {
                println!("Received message: {:?}", msg);
            }
            Err(e) => {
                println!("Error: {:?}", e);
                break;
            }
        }
    }
}
