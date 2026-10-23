// Placeholder input handling for keyboard/mouse
pub fn encode_keyboard_scancode(_key: u8) -> [u8; 2] {
    [0, _key]
}

pub fn encode_mouse_move(x: i16, y: i16) -> [u8; 4] {
    let xb = x.to_be_bytes();
    let yb = y.to_be_bytes();
    [xb[0], xb[1], yb[0], yb[1]]
}
