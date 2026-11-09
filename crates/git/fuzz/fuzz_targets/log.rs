#![no_main]

use maomi_git::parse::log::{self, LogParser};
use libfuzzer_sys::fuzz_target;

// The first byte picks the chunk size, so the fuzzer also explores split points.
fuzz_target!(|data: &[u8]| {
    let Some((&size, input)) = data.split_first() else {
        return;
    };
    let whole = log::parse(input);
    let mut parser = LogParser::new();
    let mut commits = Vec::new();
    for chunk in input.chunks(usize::from(size).max(1)) {
        match parser.push(chunk) {
            Ok(batch) => commits.extend(batch),
            Err(_) => return,
        }
    }
    if let (Ok(rest), Ok(whole)) = (parser.finish(), whole) {
        commits.extend(rest);
        assert_eq!(commits, whole, "chunked and whole parses differ");
    }
});
