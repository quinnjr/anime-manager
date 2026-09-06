//! Dev aid: walk a root with the real scanner and print one line per video with the parse result.
//! Usage: cargo run --example parse_dump -- /path/to/anime
use std::path::Path;
fn main() {
    let root = std::env::args().nth(1).expect("root path");
    let (files, errors) = anime_manager_lib::scanner::scan_dir(Path::new(&root), &mut |_| {});
    for e in &errors { eprintln!("ERR\t{e}"); }
    for f in &files {
        match anime_manager_lib::parser::parse(&f.stem, &f.dirs) {
            Some(r) => println!("OK\t{}\tS{}\tE{}\t{}\t{}", r.title, r.season, r.episode, r.release_group.unwrap_or_default(), f.path.display()),
            None => println!("FAIL\t\t\t\t\t{}", f.path.display()),
        }
    }
}
