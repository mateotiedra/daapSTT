use super::*;

#[test]
fn endpoint_has_required_query_and_encoded_repeated_keyterms() {
    let url = build_realtime_url(
        "wss://api.example.test/realtime?existing=yes",
        &["alpha beta".to_string(), "C++/Rust".to_string()],
        false,
    )
    .unwrap();
    assert_eq!(
        url.as_str(),
        "wss://api.example.test/realtime?existing=yes&model_id=scribe_v2_realtime&audio_format=pcm_16000&commit_strategy=vad&no_verbatim=true&keyterms=alpha+beta&keyterms=C%2B%2B%2FRust"
    );
}

#[test]
fn endpoint_caps_keyterms_at_fifty() {
    let keyterms = (0..51).map(|value| value.to_string()).collect::<Vec<_>>();
    let url = build_realtime_url("wss://api.example.test/realtime", &keyterms, false).unwrap();
    assert_eq!(
        url.query_pairs()
            .filter(|(key, _)| key == "keyterms")
            .count(),
        MAX_KEYTERMS
    );
}

#[test]
fn audio_serialization_encodes_pcm_and_optional_commit() {
    assert_eq!(
        serialize_audio_chunk(&[0, 1, 255], false).unwrap(),
        r#"{"message_type":"input_audio_chunk","audio_base_64":"AAH/"}"#
    );
    assert_eq!(
        serialize_audio_chunk(&[], true).unwrap(),
        r#"{"message_type":"input_audio_chunk","audio_base_64":"","commit":true}"#
    );
}

#[test]
fn inbound_messages_are_classified() {
    assert_eq!(
        parse_incoming_message(r#"{"message_type":"session_started"}"#).unwrap(),
        IncomingMessage::SessionStarted
    );
    assert_eq!(
        parse_incoming_message(r#"{"message_type":"partial_transcript","text":"draft"}"#).unwrap(),
        IncomingMessage::PartialTranscript("draft".to_string())
    );
    assert_eq!(
        parse_incoming_message(r#"{"message_type":"partial_transcript","text":""}"#).unwrap(),
        IncomingMessage::PartialTranscript(String::new())
    );
    assert_eq!(
        parse_incoming_message(r#"{"message_type":"committed_transcript","text":" final "}"#)
            .unwrap(),
        IncomingMessage::CommittedTranscript(" final ".to_string())
    );
    assert_eq!(
        parse_incoming_message(
            r#"{"message_type":"error","error":{"code":"rate_limited","message":"slow down"}}"#
        )
        .unwrap(),
        IncomingMessage::ProviderError {
            code: Some("rate_limited".to_string()),
            message: "slow down".to_string(),
        }
    );
    assert_eq!(
        parse_incoming_message(r#"{"message_type":"auth_error","message":"invalid key"}"#).unwrap(),
        IncomingMessage::ProviderError {
            code: Some("auth_error".to_string()),
            message: "invalid key".to_string(),
        }
    );
    assert_eq!(
        parse_incoming_message(r#"{"message_type":"rate_limited","error":"slow down"}"#).unwrap(),
        IncomingMessage::ProviderError {
            code: Some("rate_limited".to_string()),
            message: "slow down".to_string(),
        }
    );
}

#[test]
fn partial_transcripts_are_emitted_including_empty_text() {
    let (events, mut received) = mpsc::unbounded_channel();
    for text in ["draft", ""] {
        assert_eq!(
            handle_socket_message(
                Some(Ok(Message::Text(
                    format!(r#"{{"message_type":"partial_transcript","text":"{text}"}}"#).into()
                ))),
                &events,
            ),
            SocketOutcome::Continue,
        );
        assert_eq!(
            received.try_recv().unwrap(),
            RealtimeEvent::PartialTranscript(text.to_string())
        );
    }
}

#[test]
fn malformed_messages_are_rejected_and_blank_commits_are_ignored() {
    assert_eq!(
        parse_incoming_message("not json"),
        Err(RealtimeError::InvalidMessage)
    );
    let (events, mut received) = mpsc::unbounded_channel();
    assert_eq!(
        handle_socket_message(
            Some(Ok(Message::Text(
                r#"{"message_type":"committed_transcript","text":"  "}"#.into()
            ))),
            &events,
        ),
        SocketOutcome::Committed,
    );
    assert!(received.try_recv().is_err());
}

#[tokio::test]
async fn manual_release_drains_queued_audio_and_finishes_on_final_not_socket_close() {
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    for final_text in ["bonjour corrigé", ""] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
        let (release_server, keep_open) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut audio = Vec::new();
            loop {
                let Message::Text(payload) = socket.next().await.unwrap().unwrap() else {
                    panic!("expected audio");
                };
                let chunk: serde_json::Value = serde_json::from_str(&payload).unwrap();
                audio.extend(STANDARD.decode(chunk["audio_base_64"].as_str().unwrap()).unwrap());
                if chunk["commit"] == true {
                    assert_eq!(audio, [1, 2, 3, 4]);
                    socket.send(Message::Text(
                        serde_json::json!({
                            "message_type": "committed_transcript", "text": final_text,
                        }).to_string().into(),
                    )).await.unwrap();
                    // Completion must not depend on the provider closing its socket.
                    let _ = keep_open.await;
                    break;
                }
            }
        });
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        audio_tx.send(vec![1, 2]).unwrap();
        audio_tx.send(vec![3, 4]).unwrap();
        drop(audio_tx);
        let mut session = RealtimeSession::start(RealtimeConfig {
            endpoint, api_key: "test".into(), keyterms: vec![], manual_commits: true,
        }, audio_rx).unwrap();
        session.send_control(RealtimeControl::Finalize).unwrap();
        let events = timeout(Duration::from_secs(1), async {
            let mut events = Vec::new();
            while let Some(event) = session.recv().await {
                events.push(event);
            }
            events
        }).await.expect("final acknowledgement should end the session without the 3s grace period");
        let expected = if final_text.is_empty() {
            vec![RealtimeEvent::Completed]
        } else {
            vec![RealtimeEvent::CommittedTranscript(final_text.into()), RealtimeEvent::Completed]
        };
        assert_eq!(events, expected);
        let _ = release_server.send(());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn manual_long_dictation_waits_for_each_segment_and_keeps_the_final_tail() {
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let (allow_final, final_allowed) = oneshot::channel();
    let (release_server, keep_open) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut accumulated = 0;
        let mut commits = 0;
        let mut final_allowed = Some(final_allowed);
        loop {
            let Message::Text(payload) = socket.next().await.unwrap().unwrap() else {
                panic!("expected audio");
            };
            let chunk: serde_json::Value = serde_json::from_str(&payload).unwrap();
            let audio = STANDARD.decode(chunk["audio_base_64"].as_str().unwrap()).unwrap();
            assert!(audio.iter().all(|byte| *byte == 7));
            accumulated += audio.len();
            assert!(accumulated <= MANUAL_SEGMENT_BYTES);
            if chunk["commit"] == true {
                commits += 1;
                let text = if commits == 1 {
                    assert_eq!(accumulated, MANUAL_SEGMENT_BYTES);
                    "début"
                } else {
                    assert_eq!(commits, 2);
                    assert_eq!(accumulated, 4);
                    final_allowed.take().unwrap().await.unwrap();
                    "derniers mots"
                };
                socket.send(Message::Text(
                    serde_json::json!({
                        "message_type": "committed_transcript", "text": text,
                    }).to_string().into(),
                )).await.unwrap();
                accumulated = 0;
                if commits == 2 {
                    let _ = keep_open.await;
                    break;
                }
            }
        }
    });
    let (audio_tx, audio_rx) = mpsc::unbounded_channel();
    // Cross a segment boundary inside one queued frame.
    audio_tx.send(vec![7; MANUAL_SEGMENT_BYTES + 4]).unwrap();
    drop(audio_tx);
    let mut session = RealtimeSession::start(RealtimeConfig {
        endpoint, api_key: "test".into(), keyterms: vec![], manual_commits: true,
    }, audio_rx).unwrap();
    session.send_control(RealtimeControl::Finalize).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), session.recv()).await.unwrap(),
        Some(RealtimeEvent::CommittedTranscript("début".into())),
    );
    // The first acknowledged segment must not finish the whole dictation.
    assert!(session.events.try_recv().is_err());
    allow_final.send(()).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), session.recv()).await.unwrap(),
        Some(RealtimeEvent::CommittedTranscript("derniers mots".into())),
    );
    assert_eq!(session.recv().await, Some(RealtimeEvent::Completed));
    assert_eq!(session.recv().await, None);
    let _ = release_server.send(());
    server.await.unwrap();
}
