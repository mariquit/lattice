/// Arithmetic noise body — lexically rank 1 for query "token validation"
/// (function name matches exactly), semantically weak (body is just math).
pub fn token_validation(x: i32, y: i32) -> i32 {
    let a = x * 2 + y;
    let b = a * 3 - x;
    let c = b + a * y;
    let d = c * x - b;
    let e = d + c * 2;
    e
}
