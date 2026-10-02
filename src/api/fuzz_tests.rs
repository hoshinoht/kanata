use super::wire::ChatWire;
use crate::adapter::transport::sse::SseFramer;

fn framed(bytes: &[u8], chunk: usize) -> Result<Vec<(String, Option<String>)>, ()> {
    let mut parser = SseFramer::new_with_named_events();
    let mut records = Vec::new();
    for part in bytes.chunks(chunk) {
        records.extend(
            parser
                .feed(part)
                .map_err(|_| ())?
                .into_iter()
                .map(|record| (record.data, record.event)),
        );
    }
    parser.finish().map_err(|_| ())?;
    Ok(records)
}

#[test]
fn parser_mutation_smoke() {
    let corpus: &[&[u8]] = &[
        br#"{"model":"fixture","messages":[{"role":"user","content":"hello"}]}"#,
        br#"{"model":"fixture","messages":[{"role":"user","content":[{"type":"input_audio","input_audio":{"data":"YQ==","format":"wav"}}]}]}"#,
        b"data: {\"choices\":[]}\n\nevent: done\ndata: [DONE]\n\n",
        b"\xef\xbb\xbfdata: one\r\ndata: two\r\n\r\n",
    ];
    let cases = std::env::var("KANATA_FUZZ_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000usize)
        .min(1_000_000);
    let mut random = 0x8c37d4a61f09b225u64;
    for i in 0..cases {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let mut bytes = corpus[i % corpus.len()].to_vec();
        let at = random as usize % bytes.len();
        match i % 4 {
            0 => bytes[at] = (random >> 32) as u8,
            1 => {
                bytes.remove(at);
            }
            2 => bytes.insert(at, (random >> 16) as u8),
            _ => bytes.truncate(at),
        }
        if let Ok(wire) = serde_json::from_slice::<ChatWire>(&bytes) {
            let _ = wire.into_core(1024);
        }
        assert_eq!(
            framed(&bytes, bytes.len().max(1)),
            framed(&bytes, 1 + i % 17),
            "case {i}"
        );
    }
}
