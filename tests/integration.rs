//! End-to-end integration tests for whispr.
//!
//! These exercise the real crypto, session and directory code paths without any
//! Tor/onion involvement:
//! - the Noise handshake over an in-memory duplex,
//! - encrypted message delivery through the session layer,
//! - rejection of a peer whose key does not match the one we asked for,
//! - the directory server register/lookup protocol over localhost TCP,
//! - the full "connect by key via directory" flow between two `App`s.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;

use whispr::app::{App, AppEvent};
use whispr::config::{NetworkMode, TransportMode};
use whispr::crypto::noise;
use whispr::directory::client::DirectoryClient;
use whispr::directory::server;
use whispr::identity::{fingerprint_of, Identity};
use whispr::session::ConnectionManager;
use whispr::transport::RawConn;

/// Find a currently-free localhost TCP port (best-effort; tiny TOCTOU window).
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// Drain events from `rx` until `pred` matches one, or time out.
async fn wait_for<F>(rx: &mut mpsc::Receiver<AppEvent>, mut pred: F) -> Option<AppEvent>
where
    F: FnMut(&AppEvent) -> bool,
{
    let fut = async {
        while let Some(event) = rx.recv().await {
            if pred(&event) {
                return Some(event);
            }
        }
        None
    };
    timeout(Duration::from_secs(10), fut).await.ok().flatten()
}

#[tokio::test]
async fn noise_handshake_authenticates_both_keys() {
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let (mut a_side, mut b_side) = tokio::io::duplex(8192);

    let alice_priv = alice.private.clone();
    let responder = tokio::spawn(async move {
        noise::handshake_responder(&mut a_side, &alice_priv).await.unwrap()
    });

    let initiator = noise::handshake_initiator(&mut b_side, &bob.private)
        .await
        .unwrap();
    let responder = responder.await.unwrap();

    // Each side must learn the other's real static public key.
    assert_eq!(initiator.remote_public, alice.public);
    assert_eq!(responder.remote_public, bob.public);
}

#[tokio::test]
async fn session_delivers_encrypted_message() {
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let bob_fp = fingerprint_of(&bob.public);

    let (a_tx, mut a_rx) = mpsc::channel::<AppEvent>(64);
    let (b_tx, mut _b_rx) = mpsc::channel::<AppEvent>(64);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    let (a_side, b_side) = tokio::io::duplex(8192);
    let a_conn: RawConn = Box::new(a_side);
    let b_conn: RawConn = Box::new(b_side);

    let inbound = tokio::spawn(async move { mgr_a.handle_inbound(a_conn).await });
    mgr_b.connect_conn(b_conn).await;
    inbound.await.unwrap();

    mgr_b.broadcast("ping over an encrypted channel".to_string()).await;

    let event = wait_for(&mut a_rx, |e| matches!(e, AppEvent::Message { .. }))
        .await
        .expect("alice should receive a message");
    match event {
        AppEvent::Message { from, text, broadcast, .. } => {
            assert_eq!(from, bob_fp);
            assert_eq!(text, "ping over an encrypted channel");
            assert!(broadcast, "a broadcast must arrive flagged as a broadcast");
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn direct_and_broadcast_messages_are_distinguishable() {
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    // Bob addresses Alice by her fingerprint from his peer table.
    let alice_fp = fingerprint_of(&alice.public);

    let (a_tx, mut a_rx) = mpsc::channel::<AppEvent>(64);
    let (b_tx, mut _b_rx) = mpsc::channel::<AppEvent>(64);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    let (a_side, b_side) = tokio::io::duplex(8192);
    let a_conn: RawConn = Box::new(a_side);
    let b_conn: RawConn = Box::new(b_side);

    let inbound = tokio::spawn(async move { mgr_a.handle_inbound(a_conn).await });
    mgr_b.connect_conn(b_conn).await;
    inbound.await.unwrap();

    // A directed send must arrive as a 1:1 message (broadcast == false).
    mgr_b
        .send_to(&alice_fp, "just for you".to_string())
        .await
        .unwrap();
    let direct = wait_for(&mut a_rx, |e| matches!(e, AppEvent::Message { .. }))
        .await
        .expect("alice should receive the direct message");
    assert!(
        matches!(direct, AppEvent::Message { ref text, broadcast: false, .. } if text == "just for you"),
        "unexpected: {direct:?}"
    );

    // A broadcast must arrive flagged (broadcast == true).
    mgr_b.broadcast("to everyone".to_string()).await;
    let bcast = wait_for(&mut a_rx, |e| matches!(e, AppEvent::Message { .. }))
        .await
        .expect("alice should receive the broadcast");
    assert!(
        matches!(bcast, AppEvent::Message { ref text, broadcast: true, .. } if text == "to everyone"),
        "unexpected: {bcast:?}"
    );
}

#[tokio::test]
async fn a_delivered_message_is_acknowledged() {
    // The sender must receive a delivery receipt (matching the message id and the
    // recipient) once the peer acks a direct message.
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let alice_fp = fingerprint_of(&alice.public);

    let (a_tx, mut _a_rx) = mpsc::channel::<AppEvent>(64);
    let (b_tx, mut b_rx) = mpsc::channel::<AppEvent>(64);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    let (a_side, b_side) = tokio::io::duplex(8192);
    let inbound = tokio::spawn(async move { mgr_a.handle_inbound(Box::new(a_side)).await });
    mgr_b.connect_conn(Box::new(b_side)).await;
    inbound.await.unwrap();

    let id = mgr_b
        .send_to(&alice_fp, "did you get this?".to_string())
        .await
        .unwrap();

    let receipt = wait_for(&mut b_rx, |e| matches!(e, AppEvent::Delivered { .. })).await;
    assert!(
        matches!(receipt, Some(AppEvent::Delivered { id: got, peer }) if got == id && peer == alice_fp),
        "sender should get a Delivered receipt for the message it sent"
    );
}

#[tokio::test]
async fn is_connected_tracks_live_sessions() {
    // The reconnect logic relies on is_connected to know when to stop retrying.
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let bob_fp = fingerprint_of(&bob.public);

    let (a_tx, mut a_rx) = mpsc::channel::<AppEvent>(64);
    let (b_tx, mut _b_rx) = mpsc::channel::<AppEvent>(64);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    assert!(!mgr_a.is_connected(&bob_fp).await, "no session yet");

    let (a_side, b_side) = tokio::io::duplex(8192);
    let mgr_a2 = mgr_a.clone();
    let inbound = tokio::spawn(async move { mgr_a2.handle_inbound(Box::new(a_side)).await });
    mgr_b.connect_conn(Box::new(b_side)).await;
    inbound.await.unwrap();

    wait_for(&mut a_rx, |e| matches!(e, AppEvent::PeerConnected { .. })).await;
    assert!(mgr_a.is_connected(&bob_fp).await, "session should be live");
}

#[tokio::test(start_paused = true)]
async fn handshake_aborts_against_a_silent_peer() {
    // A peer that connects but never speaks must not pin us forever: the
    // handshake has to abort once HANDSHAKE_TIMEOUT elapses. Paused time makes
    // the timer fire instantly instead of waiting the real 10 seconds.
    let bob = Identity::generate("bob").unwrap();
    // Keep the far end alive but silent so our read blocks (rather than EOFs).
    let (mut b_side, _silent_peer) = tokio::io::duplex(8192);

    let result = noise::handshake_initiator(&mut b_side, &bob.private).await;
    assert!(
        result.is_err(),
        "handshake against a silent peer must time out, not hang"
    );
}

#[tokio::test]
async fn graceful_disconnect_delivers_message_then_ends_session() {
    // Regression test for the one-shot `send` RST race: a peer that sends a
    // message and then a graceful `Bye` must have its message delivered, and the
    // sender must observe the session ending (the peer closed after the Bye).
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let alice_fp = fingerprint_of(&alice.public);

    let (a_tx, mut a_rx) = mpsc::channel::<AppEvent>(64);
    let (b_tx, mut b_rx) = mpsc::channel::<AppEvent>(64);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    let (a_side, b_side) = tokio::io::duplex(8192);
    let inbound = tokio::spawn(async move { mgr_a.handle_inbound(Box::new(a_side)).await });
    mgr_b.connect_conn(Box::new(b_side)).await;
    inbound.await.unwrap();

    // Bob sends a final message and then says goodbye.
    mgr_b.send_to(&alice_fp, "last words".to_string()).await.unwrap();
    mgr_b.disconnect(&alice_fp).await;

    // Alice must still receive the message (delivered before the close)...
    let msg = wait_for(&mut a_rx, |e| matches!(e, AppEvent::Message { .. })).await;
    assert!(
        matches!(msg, Some(AppEvent::Message { ref text, .. }) if text == "last words"),
        "message must be delivered before the graceful close"
    );

    // ...and Bob must observe the session ending after his Bye (Alice closed).
    let ended = wait_for(&mut b_rx, |e| matches!(e, AppEvent::PeerDisconnected { .. })).await;
    assert!(ended.is_some(), "sender should see the session end after Bye");
}

#[tokio::test]
async fn session_rejects_key_mismatch() {
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let impostor = Identity::generate("impostor").unwrap();

    let (a_tx, mut _a_rx) = mpsc::channel::<AppEvent>(64);
    let (b_tx, mut b_rx) = mpsc::channel::<AppEvent>(64);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    let (a_side, b_side) = tokio::io::duplex(8192);
    let a_conn: RawConn = Box::new(a_side);
    let b_conn: RawConn = Box::new(b_side);

    // Alice answers, but Bob expected to reach the impostor's key.
    let inbound = tokio::spawn(async move { mgr_a.handle_inbound(a_conn).await });
    let expected_wrong = impostor.public.clone();
    mgr_b
        .connect_conn_expecting(b_conn, Some(expected_wrong))
        .await;
    inbound.await.unwrap();

    // Bob must surface an error and must NOT report a connected peer.
    let got_error = wait_for(&mut b_rx, |e| matches!(e, AppEvent::Error(_)))
        .await
        .is_some();
    assert!(got_error, "bob should report an identity-mismatch error");
    assert!(
        mgr_b.peer_list().await.is_empty(),
        "bob must not have an established peer after a key mismatch"
    );
}

#[tokio::test]
async fn directory_register_and_lookup() {
    let port = free_port();
    let bind = format!("127.0.0.1:{port}");
    tokio::spawn(async move {
        let _ = server::run(&bind).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let client = DirectoryClient::new(format!("127.0.0.1:{port}"));

    let recorded = client
        .register(&alice.private, 5555)
        .await
        .expect("register should succeed");
    assert_eq!(recorded, "127.0.0.1:5555");

    let found = client.lookup(&alice.public_b58()).await.unwrap();
    assert_eq!(found, Some("127.0.0.1:5555".to_string()));

    // An unregistered key resolves to nothing.
    let missing = client.lookup(&bob.public_b58()).await.unwrap();
    assert_eq!(missing, None);
}

#[tokio::test]
async fn connect_by_key_via_directory_end_to_end() {
    let dir_port = free_port();
    let dir_bind = format!("127.0.0.1:{dir_port}");
    let dir_addr = format!("127.0.0.1:{dir_port}");
    tokio::spawn(async move {
        let _ = server::run(&dir_bind).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let alice_pub = alice.public_b58();
    let alice_fp = alice.fingerprint();

    let a_port = free_port();
    let mut app_a = App::new(
        alice,
        TransportMode::Internet,
        NetworkMode::Direct,
        a_port,
        Some(dir_addr.clone()),
    )
    .await
    .unwrap();

    // Wait until Alice has registered with the directory.
    let registered = wait_for(&mut app_a.events, |e| {
        matches!(e, AppEvent::Info(m) if m.contains("registered"))
    })
    .await;
    assert!(registered.is_some(), "alice should register with the directory");

    let mut app_b = App::new(
        bob,
        TransportMode::Internet,
        NetworkMode::Direct,
        free_port(),
        Some(dir_addr),
    )
    .await
    .unwrap();

    // Bob reaches Alice using only her public key.
    app_b.connect_by_key(&alice_pub).await.unwrap();

    let connected = wait_for(&mut app_b.events, |e| {
        matches!(e, AppEvent::PeerConnected { .. })
    })
    .await;
    assert!(connected.is_some(), "bob should connect to alice by key");

    app_b
        .send(&alice_fp, "found you by key, not IP".to_string())
        .await
        .unwrap();

    let event = wait_for(&mut app_a.events, |e| matches!(e, AppEvent::Message { .. }))
        .await
        .expect("alice should receive bob's message");
    match event {
        AppEvent::Message { text, .. } => assert_eq!(text, "found you by key, not IP"),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn internet_direct_connect_without_directory_or_tor() {
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let alice_fp = alice.fingerprint();

    // Alice listens on the Internet transport with direct routing (no proxy).
    let a_port = free_port();
    let mut app_a = App::new(
        alice,
        TransportMode::Internet,
        NetworkMode::Direct,
        a_port,
        None,
    )
    .await
    .unwrap();
    // Give Alice's listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut app_b = App::new(
        bob,
        TransportMode::Internet,
        NetworkMode::Direct,
        free_port(),
        None,
    )
    .await
    .unwrap();

    // Bob dials Alice's address directly — no directory, no Tor.
    app_b
        .connect(&format!("127.0.0.1:{a_port}"))
        .await
        .unwrap();

    let connected = wait_for(&mut app_b.events, |e| {
        matches!(e, AppEvent::PeerConnected { .. })
    })
    .await;
    assert!(connected.is_some(), "bob should connect directly to alice");

    app_b
        .send(&alice_fp, "direct, no tor, no directory".to_string())
        .await
        .unwrap();

    let event = wait_for(&mut app_a.events, |e| matches!(e, AppEvent::Message { .. }))
        .await
        .expect("alice should receive bob's direct message");
    match event {
        AppEvent::Message { text, .. } => assert_eq!(text, "direct, no tor, no directory"),
        other => panic!("unexpected event: {other:?}"),
    }
}

#[tokio::test]
async fn hub_handles_two_peers_individually() {
    // Alice is a hub; Bob and Carol both connect to her at the same time.
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let carol = Identity::generate("carol").unwrap();
    let alice_fp = alice.fingerprint();
    let bob_fp = bob.fingerprint();
    let carol_fp = carol.fingerprint();

    let a_port = free_port();
    let mut app_a = App::new(alice, TransportMode::Internet, NetworkMode::Direct, a_port, None)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    let app_b = App::new(bob, TransportMode::Internet, NetworkMode::Direct, free_port(), None)
        .await
        .unwrap();
    let app_c = App::new(carol, TransportMode::Internet, NetworkMode::Direct, free_port(), None)
        .await
        .unwrap();

    app_b.connect(&format!("127.0.0.1:{a_port}")).await.unwrap();
    app_c.connect(&format!("127.0.0.1:{a_port}")).await.unwrap();

    // Alice should see both peers connect (this is the case that used to break).
    let mut connected = std::collections::HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while connected.len() < 2 {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = app_a.events.recv() => match ev {
                Some(AppEvent::PeerConnected { fingerprint, .. }) => { connected.insert(fingerprint); }
                Some(_) => {}
                None => break,
            }
        }
    }
    assert!(connected.contains(&bob_fp), "alice should see bob");
    assert!(connected.contains(&carol_fp), "alice should see carol");

    // Each sends to Alice; she must receive both, attributed to the right sender.
    app_b.send(&alice_fp, "hi from bob".to_string()).await.unwrap();
    app_c.send(&alice_fp, "hi from carol".to_string()).await.unwrap();

    let mut got_bob = false;
    let mut got_carol = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !(got_bob && got_carol) {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            ev = app_a.events.recv() => match ev {
                Some(AppEvent::Message { from, text, .. }) => {
                    if from == bob_fp && text == "hi from bob" { got_bob = true; }
                    if from == carol_fp && text == "hi from carol" { got_carol = true; }
                }
                Some(_) => {}
                None => break,
            }
        }
    }
    assert!(got_bob, "alice should receive bob's message");
    assert!(got_carol, "alice should receive carol's message");
}


#[tokio::test]
async fn heavy_two_way_traffic_over_a_tiny_pipe_never_drops() {
    // Regression test: frames used to be read inside `select!`, so an outgoing
    // message or heartbeat firing mid-frame discarded half-read bytes, broke
    // decryption and dropped the session. A 64-byte pipe forces every frame to
    // arrive in fragments while both sides are sending at the same time.
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let alice_fp = fingerprint_of(&alice.public);
    let bob_fp = fingerprint_of(&bob.public);

    let (a_tx, mut a_rx) = mpsc::channel::<AppEvent>(1024);
    let (b_tx, mut b_rx) = mpsc::channel::<AppEvent>(1024);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    let (a_side, b_side) = tokio::io::duplex(64);
    let mgr_a2 = mgr_a.clone();
    let inbound = tokio::spawn(async move { mgr_a2.handle_inbound(Box::new(a_side)).await });
    mgr_b.connect_conn(Box::new(b_side)).await;
    inbound.await.unwrap();

    const N: usize = 200;
    let (ma, mb) = (mgr_a.clone(), mgr_b.clone());
    let (bfp, afp) = (bob_fp.clone(), alice_fp.clone());
    let send_a = tokio::spawn(async move {
        for i in 0..N {
            ma.send_to(&bfp, format!("a{i} {}", "x".repeat(i % 50))).await.unwrap();
            if i % 3 == 0 {
                ma.broadcast(format!("ab{i}")).await;
            }
        }
    });
    let send_b = tokio::spawn(async move {
        for i in 0..N {
            mb.send_to(&afp, format!("b{i} {}", "y".repeat(i % 50))).await.unwrap();
        }
    });
    send_a.await.unwrap();
    send_b.await.unwrap();

    // Every message must arrive, and neither side may see a disconnect.
    let (b_got, b_dropped) = count_messages(&mut b_rx, N + N.div_ceil(3)).await;
    let (a_got, a_dropped) = count_messages(&mut a_rx, N).await;
    assert!(!a_dropped && !b_dropped, "session must not drop under load");
    assert_eq!(b_got, N + N.div_ceil(3), "bob must receive every message");
    assert_eq!(a_got, N, "alice must receive every message");
    assert!(mgr_a.is_connected(&bob_fp).await && mgr_b.is_connected(&alice_fp).await);
}

#[tokio::test]
async fn simultaneous_dial_keeps_one_shared_session() {
    // Both peers dial each other at the same moment (e.g. both auto-reconnect).
    // They must settle on the SAME connection; otherwise each closes the one the
    // other kept and the chat drops.
    let alice = Identity::generate("alice").unwrap();
    let bob = Identity::generate("bob").unwrap();
    let alice_fp = fingerprint_of(&alice.public);
    let bob_fp = fingerprint_of(&bob.public);

    let (a_tx, mut a_rx) = mpsc::channel::<AppEvent>(256);
    let (b_tx, mut b_rx) = mpsc::channel::<AppEvent>(256);
    let mgr_a = ConnectionManager::new(Arc::new(alice), a_tx);
    let mgr_b = ConnectionManager::new(Arc::new(bob), b_tx);

    // Connection 1: alice dials bob. Connection 2: bob dials alice.
    let (a1, b1) = tokio::io::duplex(8192);
    let (a2, b2) = tokio::io::duplex(8192);
    let (ma, mb) = (mgr_a.clone(), mgr_b.clone());
    let (ma2, mb2) = (mgr_a.clone(), mgr_b.clone());
    tokio::join!(
        ma.connect_conn(Box::new(a1)),
        mb.handle_inbound(Box::new(b1)),
        mb2.connect_conn(Box::new(b2)),
        ma2.handle_inbound(Box::new(a2)),
    );

    // Let the losing duplicate close and any teardown grace period pass.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(mgr_a.is_connected(&bob_fp).await, "alice must still be connected");
    assert!(mgr_b.is_connected(&alice_fp).await, "bob must still be connected");

    // And the surviving session must actually carry messages both ways.
    mgr_a.send_to(&bob_fp, "hi bob".to_string()).await.unwrap();
    mgr_b.send_to(&alice_fp, "hi alice".to_string()).await.unwrap();
    let got_b = wait_for(&mut b_rx, |e| matches!(e, AppEvent::Message { text, .. } if text == "hi bob")).await;
    let got_a = wait_for(&mut a_rx, |e| matches!(e, AppEvent::Message { text, .. } if text == "hi alice")).await;
    assert!(got_b.is_some() && got_a.is_some(), "messages must flow on the kept session");
}

/// Count chat messages until `want` arrive (or 20s pass), noting any disconnect.
async fn count_messages(rx: &mut mpsc::Receiver<AppEvent>, want: usize) -> (usize, bool) {
    let mut got = 0;
    let mut dropped = false;
    let _ = timeout(Duration::from_secs(20), async {
        while got < want {
            match rx.recv().await {
                Some(AppEvent::Message { .. }) => got += 1,
                Some(AppEvent::PeerDisconnected { .. }) => dropped = true,
                Some(_) => {}
                None => break,
            }
        }
    })
    .await;
    (got, dropped)
}
