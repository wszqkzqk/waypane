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

pub fn ascii_to_evdev(text: &str) -> Option<Vec<(u32, bool)>> {
    const LETTERS: [u32; 26] = [
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    let mut keys = Vec::new();
    for character in text.chars() {
        let key = match character {
            'a'..='z' => (LETTERS[(character as u8 - b'a') as usize], false),
            'A'..='Z' => (LETTERS[(character as u8 - b'A') as usize], true),
            '0' => (11, false),
            '1'..='9' => (u32::from(character as u8 - b'0' + 1), false),
            ' ' => (57, false),
            '\n' | '\r' => (28, false),
            '\t' => (15, false),
            '\u{8}' => (14, false),
            '-' => (12, false),
            '=' => (13, false),
            '[' => (26, false),
            ']' => (27, false),
            '\\' => (43, false),
            ';' => (39, false),
            '\'' => (40, false),
            '`' => (41, false),
            ',' => (51, false),
            '.' => (52, false),
            '/' => (53, false),
            '!' => (2, true),
            '@' => (3, true),
            '#' => (4, true),
            '$' => (5, true),
            '%' => (6, true),
            '^' => (7, true),
            '&' => (8, true),
            '*' => (9, true),
            '(' => (10, true),
            ')' => (11, true),
            '_' => (12, true),
            '+' => (13, true),
            '{' => (26, true),
            '}' => (27, true),
            '|' => (43, true),
            ':' => (39, true),
            '"' => (40, true),
            '~' => (41, true),
            '<' => (51, true),
            '>' => (52, true),
            '?' => (53, true),
            _ => return None,
        };
        keys.push(key);
    }
    Some(keys)
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
        assert_eq!(
            ascii_to_evdev("aB9!"),
            Some(vec![(30, false), (48, true), (10, false), (2, true)])
        );
        assert_eq!(ascii_to_evdev("你好"), None);
    }
}
