pub fn login() {
    validate_token();
    check_permission();
}

pub fn validate_token() -> bool {
    verify_payment();
    true
}

pub fn check_permission() -> bool {
    true
}
