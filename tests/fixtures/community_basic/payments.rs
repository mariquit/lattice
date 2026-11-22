pub fn charge() {
    refund();
    verify_payment();
}

pub fn refund() {
    verify_payment();
}

pub fn verify_payment() -> bool {
    true
}
