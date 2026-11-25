//! Tests of the second review of the test network: the privacy scan, the side store of blocks and
//! hand outs, reports that count only with a block and a hand out, the client checks of values that
//! come from a hub.

use sommelier::report::{find_private, scrub};

#[test]
fn privacy_scan_finds_what_the_review_named() {
    let samples = [
        (r"cache C:\Users\Jane Roe\AppData\Local\x\y.bin end", "Roe"),
        (
            r"sync C:\Users\jroe\OneDrive - Acme Corp\Games\x end",
            "Acme",
        ),
        (r"sync D:\OneDrive - Acme Corp\Games end", "Acme"),
        (r"wine Z:\home\jroe\.wine\drive_c end", "jroe"),
        (r"wine \home\jroe\.wine end", "jroe"),
        (
            r"owner S-1-5-21-1004336348-1177238915-682003330-1001 end",
            "1004336348",
        ),
        (
            r"dev {123e4567-e89b-12d3-a456-426614174000} end",
            "426614174000",
        ),
        ("adapter 00:1A:2B:3C:4D:5E up", "3C:4D"),
        ("adapter 00-1a-2b-3c-4d-5e up", "3c-4d"),
        ("peer 203.0.113.77 reset", "203.0.113"),
        ("peer 2001:db8:85a3:0:0:8a2e:370:7334 reset", "8a2e"),
        ("peer fe80::1ff:fe23:4567:890a reset", "890a"),
        ("access_token=abcd1234efgh5678ijkl", "abcd1234"),
        (r#"{"refresh_token": "abcd1234efgh5678ijkl"}"#, "abcd1234"),
        ("client_secret: abcd1234efgh5678ijkl", "abcd1234"),
    ];
    for (line, leak) in samples {
        assert!(!find_private(line).is_empty(), "no hit in: {line}");
        let clean = scrub(line);
        assert!(!clean.contains(leak), "`{leak}` left in: {clean}");
        assert!(find_private(&clean).is_empty(), "still private: {clean}");
    }
}

#[test]
fn privacy_scan_keeps_lines_that_only_look_alike() {
    let lines = [
        "listening on 127.0.0.1 port 7300",
        "bind 0.0.0.0 ok",
        "loopback ::1 ok",
        "std::vec::Vec created",
        "frame time 12:34:56 done",
        "driver 31.0.15.4601 loaded",
        "token expired at 12:30",
        "vk::Result::SUCCESS",
        "d3d12: DXGI_ERROR_DEVICE_REMOVED (0x887A0005)",
    ];
    for l in lines {
        assert!(find_private(l).is_empty(), "false hit: {l}");
        assert_eq!(scrub(l), l);
    }
}
