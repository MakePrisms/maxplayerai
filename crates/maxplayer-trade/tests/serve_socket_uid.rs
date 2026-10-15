//! Review of #1153, M1: #1140 F5 proven on the real serve socket, not only on integers.
//! Each end is given a uid that is not its peer's (the test seam `*_expecting`), standing in for
//! a second local user. No mint, relay or home lock is involved.
use maxplayer_trade::serve::{Command, PROTOCOL_V, Request, TokenText};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// serve end: a peer whose uid is not serve's is dropped before its request is read; nothing is
/// answered and nothing is queued.
#[tokio::test]
async fn serve_drops_a_peer_of_another_uid_before_reading() {
    let home = tempfile::tempdir().unwrap();
    let mut jobs = maxplayer_trade::serve::bind_expecting(
        home.path(),
        vec!["ws://127.0.0.1:1".into()],
        me() + 1,
    )
    .unwrap();
    let mut s = tokio::net::UnixStream::connect(home.path().join("serve.sock"))
        .await
        .unwrap();
    let _ = s
        .write_all(
            b"{\"v\":3,\"relays\":[\"ws://127.0.0.1:1\"],\"mint_relays\":[],\"command\":{\"cancel\":{\"lot\":\"x\"}}}\n",
        )
        .await;
    let mut reply = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_string(&mut reply)).await;
    assert!(
        reply.is_empty(),
        "SAFETY: nothing is answered to a foreign uid: {reply}"
    );
    assert!(jobs.try_recv().is_err(), "SAFETY: nothing queued");
}

/// client end: the token is never written to a serve of another uid.
#[tokio::test]
async fn client_writes_nothing_to_a_serve_of_another_uid() {
    let home = tempfile::tempdir().unwrap();
    let l = tokio::net::UnixListener::bind(home.path().join("serve.sock")).unwrap();
    let req = Request {
        v: PROTOCOL_V,
        relays: vec![],
        mint_relays: vec![],
        command: Command::Receive {
            mint: "http://127.0.0.1:1".into(),
            token: TokenText("cashuBsecret".into()),
        },
    };
    let accept = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.unwrap();
        let mut got = vec![];
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut got)).await;
        got
    });
    let e = maxplayer_trade::serve::forward_expecting(home.path(), &req, me() + 1)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("another user"), "{e:#}");
    assert!(
        accept.await.unwrap().is_empty(),
        "SAFETY: no byte (no token) reached the listener"
    );
}
