#![no_main]

use maomi_git::parse::refs;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = refs::parse_refs(data);
    let _ = refs::parse_stashes(data);
    let _ = refs::parse_remotes(data);
    let _ = refs::parse_ahead_behind(data);
});
