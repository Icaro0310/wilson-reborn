// SPDX-License-Identifier: GPL-3.0-or-later
//! Optional validation against the ORIGINAL (copyrighted) Johnny Castaway data.
//!
//! Skipped unless `WILSON_DATA_DIR` points at a directory containing `RESOURCE.MAP`
//! and the data file it references (e.g. `RESOURCE.001`). CI never has the data, so
//! this test no-ops there; run it locally with:
//!
//! ```sh
//! WILSON_DATA_DIR=/path/to/dist cargo test -p wilson-dgds --test real_data -- --nocapture
//! ```

use wilson_dgds::{Archive, ResourceMap};

#[test]
fn parses_and_decodes_real_data_if_present() {
    let Ok(dir) = std::env::var("WILSON_DATA_DIR") else {
        eprintln!("WILSON_DATA_DIR not set — skipping real-data validation");
        return;
    };

    let map = std::fs::read(format!("{dir}/RESOURCE.MAP")).expect("read RESOURCE.MAP");
    let rm = ResourceMap::parse(&map).expect("parse RESOURCE.MAP");
    let data = std::fs::read(format!("{dir}/{}", rm.data_file_name)).expect("read data file");

    let archive = Archive::parse(&map, &data).expect("parse the real archive");
    assert!(!archive.bitmaps.is_empty(), "expected BMP resources");
    assert!(!archive.screens.is_empty(), "expected SCR resources");
    assert!(!archive.ttms.is_empty(), "expected TTM resources");
    assert!(!archive.ads.is_empty(), "expected ADS resources");
    assert!(archive.palette().is_some(), "expected a palette");

    // Decompression + bytecode decoding must succeed on every script.
    for (name, ttm) in &archive.ttms {
        ttm.instructions()
            .unwrap_or_else(|e| panic!("TTM {name} failed to decode: {e}"));
    }
    for (name, ads) in &archive.ads {
        ads.instructions()
            .unwrap_or_else(|e| panic!("ADS {name} failed to decode: {e}"));
    }

    eprintln!(
        "real data OK: {} bmp, {} scr, {} ttm, {} ads",
        archive.bitmaps.len(),
        archive.screens.len(),
        archive.ttms.len(),
        archive.ads.len()
    );
}

/// Diagnostic: which BMPs does each TTM load (F02F LOAD_BMP string args)?
/// `WILSON_DATA_DIR=... cargo test -p wilson-dgds --test real_data ttm_loads -- --nocapture`
#[test]
fn ttm_loads_bmp_names() {
    let Ok(dir) = std::env::var("WILSON_DATA_DIR") else {
        return;
    };
    let map = std::fs::read(format!("{dir}/RESOURCE.MAP")).unwrap();
    let rm = ResourceMap::parse(&map).unwrap();
    let data = std::fs::read(format!("{dir}/{}", rm.data_file_name)).unwrap();
    let archive = Archive::parse(&map, &data).expect("parse the real archive");

    eprintln!("BMPs:");
    let mut names: Vec<&str> = archive.bitmaps.iter().map(|(n, _)| n.as_str()).collect();
    names.sort();
    for n in names {
        eprintln!("  {n}");
    }
    eprintln!("TTM LOAD_BMP / LOAD_SCR refs:");
    for (name, ttm) in &archive.ttms {
        let mut refs = Vec::new();
        for ins in ttm.instructions().unwrap() {
            if ins.opcode == 0xF02F || ins.opcode == 0xF01F {
                if let wilson_dgds::TtmArgs::Str(s) = &ins.args {
                    refs.push(format!("{:04X}:{}", ins.opcode, s));
                }
            }
        }
        eprintln!("  {name}: {}", refs.join(", "));
    }
}
