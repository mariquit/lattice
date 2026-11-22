pub fn validate_token(token: &str) -> bool {
    check_signature(token)
}

pub fn check_signature(token: &str) -> bool {
    token.len() > 8
}
