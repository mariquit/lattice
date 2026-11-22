use crate::hub;

pub fn use_a() -> u32 {
    hub::alpha() + hub::beta()
}
