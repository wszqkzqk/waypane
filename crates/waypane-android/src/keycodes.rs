pub fn android_keycode_to_evdev(code: i32) -> Option<u32> {
    Some(match code {
        111 | 4 => 1,
        8..=16 => u32::try_from(code - 8 + 2).ok()?,
        7 => 11,
        69 => 12,
        70 => 13,
        67 => 14,
        61 => 15,
        45 => 16,
        51 => 17,
        33 => 18,
        46 => 19,
        48 => 20,
        53 => 21,
        49 => 22,
        37 => 23,
        43 => 24,
        44 => 25,
        71 => 26,
        72 => 27,
        66 => 28,
        113 => 29,
        29 => 30,
        47 => 31,
        32 => 32,
        34 => 33,
        35 => 34,
        36 => 35,
        38 => 36,
        39 => 37,
        40 => 38,
        74 => 39,
        75 => 40,
        68 => 41,
        59 => 42,
        73 => 43,
        54 => 44,
        52 => 45,
        31 => 46,
        50 => 47,
        30 => 48,
        42 => 49,
        41 => 50,
        55 => 51,
        56 => 52,
        76 => 53,
        60 => 54,
        57 => 56,
        62 => 57,
        115 => 58,
        131..=140 => u32::try_from(code - 131 + 59).ok()?,
        143 => 69,
        116 => 70,
        157 => 78,
        159 => 83,
        160 => 96,
        114 => 97,
        161 => 98,
        120 => 99,
        58 => 100,
        3 => 102,
        19 => 103,
        92 => 104,
        21 => 105,
        22 => 106,
        123 => 107,
        20 => 108,
        93 => 109,
        124 => 110,
        112 => 111,
        121 => 119,
        117 => 125,
        118 => 126,
        82 => 127,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_real_android_keycodes_to_linux_evdev() {
        assert_eq!(android_keycode_to_evdev(29), Some(30));
        assert_eq!(android_keycode_to_evdev(30), Some(48));
        assert_eq!(android_keycode_to_evdev(54), Some(44));
        assert_eq!(android_keycode_to_evdev(45), Some(16));
        assert_eq!(android_keycode_to_evdev(51), Some(17));
        assert_eq!(android_keycode_to_evdev(59), Some(42));
        assert_eq!(android_keycode_to_evdev(19), Some(103));
        assert_eq!(android_keycode_to_evdev(8), Some(2));
        assert_eq!(android_keycode_to_evdev(16), Some(10));
        assert_eq!(android_keycode_to_evdev(999), None);
    }
}
