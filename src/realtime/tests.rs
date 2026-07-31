use super::*;

#[test]
fn endpoint_has_required_query_and_encoded_repeated_keyterms() {
    let url = build_realtime_url(
        "wss://api.example.test/realtime?existing=yes",
        &["alpha beta".to_string(), "C++/Rust".to_string()],
    )
    .unwrap();
    assert_eq!(
        url.as_str(),
        "wss://api.example.test/realtime?existing=yes&model_id=scribe_v2_realtime&audio_format=pcm_16000&commit_strategy=vad&keyterms=alpha+beta&keyterms=C%2B%2B%2FRust"
    );
}

#[test]
fn endpoint_caps_keyterms_at_fifty() {
    let keyterms = (0..51).map(|value| value.to_string()).collect::<Vec<_>>();
    let url = build_realtime_url("wss://api.example.test/realtime", &keyterms).unwrap();
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
        SocketOutcome::Continue,
    );
    assert!(received.try_recv().is_err());
}
