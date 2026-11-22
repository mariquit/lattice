/// Tests for the session record store.
///
/// Exercises persist_session_record with a variety of inputs to ensure the
/// store correctly writes and validates durable entries.

#[cfg(test)]
mod tests {
    use super::*;

    fn make_payload(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn test_persist_session_record_basic() {
        // Verify that persisting a valid session record succeeds.
        let result = persist_session_record("sess-001", &make_payload("data"));
        assert!(result.is_ok(), "expected persist to succeed");
    }

    #[test]
    fn test_persist_session_record_empty_id() {
        // Empty session_id must be rejected before writing to the store.
        let result = persist_session_record("", &make_payload("data"));
        assert!(result.is_err(), "expected empty id to fail");
    }

    #[test]
    fn test_persist_session_record_large_payload() {
        // Large payloads should persist without truncation.
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let result = persist_session_record("sess-big", &payload);
        assert!(result.is_ok(), "large payload persist failed");
    }
}
