mod auth;
mod db;
mod metrics;

fn main() {
    if auth::validate_token("abcdefghi") {
        db::save_session(1);
    }
}
