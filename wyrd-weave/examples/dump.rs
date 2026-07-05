//! Decode a wyrd recording to human-readable text: `cargo run -p wyrd-weave --example dump -- run.wyrd`
fn main() {
    let path = std::env::args().nth(1).expect("usage: dump <recording>");
    let records = wyrd_weave::read_records(&path).expect("read recording");
    println!("{} records", records.len());
    for r in &records {
        println!("[{:>10}ns] {:?}", r.ts, r.event);
    }
}
