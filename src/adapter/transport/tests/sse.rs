use crate::adapter::transport::sse::SseFramer;

#[test]
fn default_framer_still_rejects_named_event_labels() {
    let mut framer = SseFramer::new();
    assert!(
        framer
            .feed(b"event: response.created\ndata: {}\n\n")
            .is_err()
    );
}

#[test]
fn opt_in_framer_preserves_named_labels_and_multiline_data() {
    let mut framer = SseFramer::new_with_named_events();
    let records = framer
        .feed(
            b"event: response.created\ndata: {\"type\":\"response.created\",\ndata: \"response\":{}}\n\n",
        )
        .expect("named SSE event parses");

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].event.as_deref(), Some("response.created"));
    assert_eq!(
        records[0].data,
        "{\"type\":\"response.created\",\n\"response\":{}}"
    );
    framer.finish().expect("complete SSE event");
}

#[test]
fn opt_in_framer_rejects_conflicting_event_labels() {
    let mut framer = SseFramer::new_with_named_events();
    assert!(
        framer
            .feed(b"event: response.created\nevent: response.completed\ndata: {}\n\n")
            .is_err()
    );
}

#[test]
fn raised_limits_accept_large_events_that_defaults_reject() {
    let line = format!("data: {}\n\n", "x".repeat(100 * 1024));
    assert!(SseFramer::new().feed(line.as_bytes()).is_err());
    let records = SseFramer::new()
        .with_limits(4 * 1024 * 1024, 4 * 1024 * 1024)
        .feed(line.as_bytes())
        .expect("raised limits");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].data.len(), 100 * 1024);
}

#[test]
fn long_line_fed_in_small_chunks_frames_correctly() {
    let payload = "y".repeat(200 * 1024);
    let body = format!("\u{feff}data: {payload}\r\n\r\ndata: tail\n\n");
    let mut framer = SseFramer::new().with_limits(1024 * 1024, 1024 * 1024);
    let mut records = Vec::new();
    let (bom_prefix, rest) = body.as_bytes().split_at(2);
    for chunk in std::iter::once(bom_prefix).chain(rest.chunks(1021)) {
        records.extend(framer.feed(chunk).expect("chunked long line"));
    }
    framer.finish().expect("complete stream");
    let data: Vec<_> = records.iter().map(|record| record.data.as_str()).collect();
    assert_eq!(data, [payload.as_str(), "tail"]);
}
